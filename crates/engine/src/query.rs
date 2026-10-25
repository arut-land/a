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
                let at = match scope {
                    Scope::Daemon => At::Daemon,
                    Scope::Workspace(dir) => {
                        At::Workspace(Arc::from(workspace_of(dir.local(), None)))
                    }
                    Scope::Session(session) => match daemon.slot(session) {
                        Some(slot) => {
                            At::Session(session, Arc::from(slot.header.workspace.as_path()))
                        }
                        None => return failed(outbox, id, format!("no session {session}")),
                    },
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
                let answer = Box::pin(async move {
                    let updates = Paced { outbox: &paced, id };
                    let answer = query.answer(&body, &at, asked, &updates).await;
                    answer.map_err(Ended::Failed)
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
}

impl Sink for Paced<'_> {
    fn send(&self, body: Json) -> BoxFuture<'_, ()> {
        let id = self.id;
        Box::pin(async move {
            // The writer ends with the connection; a lost update needs no
            // handling, as the query is cancelled with it.
            if let Ok(frame) = Frame::new(&DaemonMsg::Update { id, body }) {
                let _ = self.outbox.send_paced(frame).await;
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
