//! Queries a connection asks the extensions, and the blobs it fetches,
//! each answered on a task of its own and never logged: the session's log
//! is not touched, and the connection goes on reading while one runs.
//! Every one admitted ends with one [`DaemonMsg::Reply`], or a fetch
//! refused with [`DaemonMsg::NotHeld`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ext::host::Registry;
use ext::query::{Asked, At, Sink};
use ext::{BoxFuture, Cancel};
use protocol::{ClientMsg, DaemonMsg, Frame, Json, MachineId, Outcome, QueryInfo, Role, Scope};

use crate::driver::{Failure, contain};
use crate::outbox::{Outbox, QueuePermit};
use crate::server::{reply, workspace_of};
use crate::{Daemon, lock};

/// A connection's running queries, by the client's ids: cancelled as the
/// client asks or, all of them, as it hangs up.
#[derive(Default)]
pub(crate) struct Queries(Arc<Mutex<BTreeMap<u64, Cancel>>>);

impl Queries {
    /// Takes a query or a fetch, answered on its own task if it is
    /// admitted, or a cancel of either; hands back any other message.
    /// `bytes` is what its frame weighed.
    pub(crate) fn take(
        &self,
        daemon: &Arc<Daemon>,
        outbox: &Outbox,
        machine: MachineId,
        role: Role,
        msg: ClientMsg,
        bytes: usize,
    ) -> Option<ClientMsg> {
        match msg {
            ClientMsg::Query {
                id,
                service,
                scope,
                body,
            } => {
                let permit = self.admit(outbox, id, bytes)?;
                let registry = daemon.shared.registry.get();
                let Some(query) = registry.query(&service) else {
                    return failed(outbox, id, format!("no query named {service}"));
                };
                let cancel = Cancel::default();
                let asked = Asked {
                    machine,
                    role,
                    cancel: cancel.clone(),
                    services: Arc::clone(&daemon.queried),
                    registry,
                };
                let paced = outbox.clone();
                let resolving = Arc::clone(daemon);
                let answer = Box::pin(async move {
                    let at = match scope {
                        Scope::Daemon => At::Daemon,
                        Scope::Workspace(dir) => {
                            let workspace = rt::unblock(&*resolving.shared.rt, move || {
                                workspace_of(dir.local(), None)
                            })
                            .await;
                            At::Workspace(Arc::from(workspace))
                        }
                        Scope::Session(session) => match resolving.slot(session) {
                            Some(slot) => {
                                At::Session(session, Arc::from(slot.header.workspace.as_path()))
                            }
                            None => return Err(Ended::Failed(format!("no session {session}"))),
                        },
                    };
                    let updates = Paced {
                        outbox: &paced,
                        id,
                        failed: Mutex::default(),
                        ended: Cancel::default(),
                    };
                    let answer = updates
                        .ended
                        .until(query.answer(&body, &at, asked, &updates))
                        .await;
                    if let Some(error) = lock(&updates.failed).take() {
                        return Err(Ended::Failed(error));
                    }
                    answer
                        .expect("only a failed update ends the stream")
                        .map_err(Ended::Failed)
                });
                self.run(daemon, outbox, id, permit, cancel, answer);
            }
            ClientMsg::Fetch {
                id,
                session,
                blob,
                offset,
                len,
            } => {
                let permit = self.admit(outbox, id, bytes)?;
                let fetch = crate::fetch::answer(
                    Arc::clone(daemon),
                    outbox.clone(),
                    session,
                    blob,
                    offset,
                    len,
                );
                self.run(
                    daemon,
                    outbox,
                    id,
                    permit,
                    Cancel::default(),
                    Box::pin(fetch),
                );
            }
            ClientMsg::Cancel { id } => {
                if let Some(cancel) = lock(&self.0).get(&id) {
                    cancel.set();
                }
            }
            msg => return Some(msg),
        }
        None
    }

    /// Admission for one more to run as `id`, which no running one has;
    /// `None` once refused, its answer sent.
    fn admit(&self, outbox: &Outbox, id: u64, bytes: usize) -> Option<QueuePermit> {
        // Not a reply: that would read as the running one's.
        if lock(&self.0).contains_key(&id) {
            let message = format!("query {id} is running already");
            reply(outbox, &DaemonMsg::Error { message });
            return None;
        }
        match QueuePermit::query(bytes) {
            Some(permit) => Some(permit),
            None => failed(outbox, id, "daemon admission is full".into()),
        }
    }

    /// Runs `answer` as `id` on a task of its own, until it ends or
    /// `cancel` is set, then sends how it ended.
    fn run(
        &self,
        daemon: &Daemon,
        outbox: &Outbox,
        id: u64,
        permit: QueuePermit,
        cancel: Cancel,
        answer: BoxFuture<'static, Result<Json, Ended>>,
    ) {
        lock(&self.0).insert(id, cancel.clone());
        let (running, outbox) = (Arc::clone(&self.0), outbox.clone());
        daemon.shared.rt.spawn(Box::pin(async move {
            let _permit = permit;
            // One that panics fails, as a tool's call does.
            let answer = contain(answer);
            // A query that sees the cancel and returns what it had (a grep
            // stopping between chunks) can finish within the same poll as
            // the cancel lands: it was interrupted all the same.
            let outcome = match cancel.until(answer).await {
                Some(Ok(output)) if !cancel.is_set() => Outcome::Done { output },
                Some(Err(Ended::Failed(error))) => Outcome::Failed { error },
                Some(Err(Ended::NotHeld)) => {
                    lock(&running).remove(&id);
                    reply(&outbox, &DaemonMsg::NotHeld { id });
                    return;
                }
                Some(Ok(_)) | None => Outcome::Interrupted { output: None },
            };
            lock(&running).remove(&id);
            reply(&outbox, &DaemonMsg::Reply { id, outcome });
        }));
    }
}

/// Answers `id` with `error`; `None`, as nothing more is taken of it.
fn failed<T>(outbox: &Outbox, id: u64, error: String) -> Option<T> {
    let outcome = Outcome::Failed { error };
    reply(outbox, &DaemonMsg::Reply { id, outcome });
    None
}

/// How one ended without its answer, other than by its cancel.
pub(crate) enum Ended {
    Failed(String),
    /// A fetch of a blob no session it names references.
    NotHeld,
}

impl Failure for Result<Json, Ended> {
    fn failed(error: String) -> Self {
        Err(Ended::Failed(error))
    }
}

impl Drop for Queries {
    fn drop(&mut self) {
        lock(&self.0).values().for_each(Cancel::set);
    }
}

/// Query `id`'s updates, each sent once its client has room for it.
struct Paced<'a> {
    outbox: &'a Outbox,
    id: u64,
    failed: Mutex<Option<String>>,
    ended: Cancel,
}

impl Sink for Paced<'_> {
    fn send(&self, body: Json) -> BoxFuture<'_, ()> {
        let id = self.id;
        Box::pin(async move {
            // The writer ends with the connection; a lost update needs no
            // handling, as the query is cancelled with it.
            match Frame::new(&DaemonMsg::Update { id, body }) {
                Ok(frame) => {
                    let _ = self.outbox.send_paced(frame).await;
                }
                Err(error) => {
                    *lock(&self.failed) = Some(format!("framing query update: {error}"));
                    self.ended.set();
                    // The enclosing race drops the query before another update.
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}

/// The queries `registry` answers, as a welcome advertises them.
pub(crate) fn advertised(registry: &Registry) -> Vec<QueryInfo> {
    registry
        .queries()
        .map(|q| QueryInfo {
            name: q.name().to_owned(),
            description: q.description().to_owned(),
            scope: q.scope(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::time::Duration;

    use ext::{Extension, Query, QueryCx, Registrar, Streamed, stream};
    use protocol::Decode;
    use serde::{Deserialize, Serialize};

    use super::*;

    struct LargeAnswer;

    #[derive(Clone, Copy, Deserialize, Serialize)]
    enum Request {
        Reply,
        Update,
        Small,
    }

    impl Query for LargeAnswer {
        const NAME: &'static str = "large-answer";
        const DESCRIPTION: &'static str = "Exercises query framing";
        type Request = Request;
        type Reply = String;

        fn answer(
            &self,
            request: Request,
            _: &QueryCx<ext::query::Daemon>,
        ) -> Streamed<String, impl Future<Output = Result<String, String>> + Send, String> {
            stream(move |mut updates| async move {
                match request {
                    Request::Reply => Ok("x".repeat(16 << 20)),
                    Request::Update => {
                        updates.send("x".repeat(16 << 20)).await;
                        updates.send("after the failed update".into()).await;
                        Ok("done".into())
                    }
                    Request::Small => Ok("small".into()),
                }
            })
        }
    }

    impl Extension for LargeAnswer {
        const NAME: &'static str = "large-answer";
        const VERSION: u32 = 1;
        fn contribute(&self, r: &mut Registrar) {
            r.query(Self);
        }
    }

    #[test]
    fn oversized_query_replies_and_updates_fail_terminally_and_release_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let home = sys::Home::from(dir.path().to_path_buf());
        let daemon = Daemon::load(
            &home,
            &crate::Config {
                extensions: |_| Registry::default().with(&LargeAnswer),
                ..crate::Config::default()
            },
        )
        .unwrap();
        let queries = Queries::default();
        let outbox = Outbox::new();
        for request in [Request::Reply, Request::Update] {
            for request in [request, Request::Small] {
                assert!(
                    queries
                        .take(
                            &daemon,
                            &outbox,
                            MachineId([0; 16]),
                            Role::Owner,
                            ClientMsg::Query {
                                id: 7,
                                service: "large-answer".into(),
                                scope: Scope::Daemon,
                                body: Json::encode(&request).unwrap()
                            },
                            64
                        )
                        .is_none()
                );
                let frame = rt::block_on(rt::or(
                    async { outbox.next(&*daemon.shared.rt).await.unwrap() },
                    async {
                        daemon.shared.rt.sleep(Duration::from_secs(10)).await;
                        panic!("query did not end")
                    },
                ));
                let DaemonMsg::Reply { id, outcome } = DaemonMsg::from_json(frame.body()).unwrap()
                else {
                    panic!("expected terminal reply")
                };
                assert_eq!(id, 7);
                match request {
                    Request::Reply | Request::Update => assert!(
                        matches!(outcome, Outcome::Failed { error } if error.contains("framing"))
                    ),
                    Request::Small => assert!(
                        matches!(outcome, Outcome::Done { output } if output.parse::<String>().unwrap() == "small")
                    ),
                }
                assert!(lock(&queries.0).is_empty());
                assert!(outbox.try_next().is_none(), "exactly one terminal reply");
            }
        }
    }
    #[test]
    fn workspace_discovery_waits_on_storage_without_delaying_queries_or_cancellation() {
        use rt::Runtime;

        let dir = tempfile::tempdir().unwrap();
        let home = sys::Home::from(dir.path().to_path_buf());
        let mut config = rt::Config::for_cores(1);
        config.storage.workers = 1;
        let runtime = Arc::new(rt::Threads::configured(config));
        let daemon = Daemon::load_on(
            &home,
            &crate::Config {
                extensions: |_| Registry::default().with(&LargeAnswer),
                ..crate::Config::default()
            },
            &runtime,
            Vec::new(),
        )
        .unwrap();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (entered, blocked) = std::sync::mpsc::channel();
        rt::block_on(runtime.spawn_blocking(
            rt::BlockingClass::Storage,
            0,
            Box::new(move || {
                entered.send(()).unwrap();
                let _ = held.recv();
            }),
        ))
        .unwrap();
        blocked.recv_timeout(Duration::from_secs(10)).unwrap();
        let queries = Queries::default();
        let outbox = Outbox::new();
        for (id, scope) in [
            (
                1,
                Scope::Workspace(dir.path().to_path_buf().try_into().unwrap()),
            ),
            (2, Scope::Daemon),
        ] {
            assert!(
                queries
                    .take(
                        &daemon,
                        &outbox,
                        MachineId([0; 16]),
                        Role::Owner,
                        ClientMsg::Query {
                            id,
                            service: "large-answer".into(),
                            scope,
                            body: Json::encode(&Request::Small).unwrap()
                        },
                        64
                    )
                    .is_none()
            );
        }
        let frame = rt::block_on(rt::or(
            async { outbox.next(&*runtime).await.unwrap() },
            async {
                runtime.sleep(Duration::from_secs(10)).await;
                panic!("async worker blocked on workspace discovery")
            },
        ));
        assert!(matches!(
            DaemonMsg::from_json(frame.body()).unwrap(),
            DaemonMsg::Reply {
                id: 2,
                outcome: Outcome::Done { .. }
            }
        ));
        queries.take(
            &daemon,
            &outbox,
            MachineId([0; 16]),
            Role::Owner,
            ClientMsg::Cancel { id: 1 },
            0,
        );
        let frame = rt::block_on(rt::or(
            async { outbox.next(&*runtime).await.unwrap() },
            async {
                runtime.sleep(Duration::from_secs(10)).await;
                panic!("workspace query did not cancel")
            },
        ));
        assert!(matches!(
            DaemonMsg::from_json(frame.body()).unwrap(),
            DaemonMsg::Reply {
                id: 1,
                outcome: Outcome::Interrupted { .. }
            }
        ));
        drop(release);
        drop(daemon);
        runtime.shutdown();
    }
}
