//! Queries a connection asks the extensions, each answered on a task of
//! its own and never logged: the session's log is not touched, and the
//! connection goes on reading while one runs. Every query admitted ends
//! with one [`DaemonMsg::Reply`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ext::host::Registry;
use ext::query::{Asked, At, Sink};
use ext::{BoxFuture, Cancel};
use protocol::{ClientMsg, DaemonMsg, Frame, Json, Outcome, QueryInfo, Role, Scope};

use crate::driver::contain;
use crate::outbox::{Outbox, QueuePermit};
use crate::server::{reply, workspace_of};
use crate::{Daemon, lock};

/// A connection's running queries, by the client's ids: cancelled as the
/// client asks or, all of them, as it hangs up.
#[derive(Default)]
pub(crate) struct Queries(Arc<Mutex<BTreeMap<u64, Cancel>>>);

impl Queries {
    /// Takes a query, answered on its own task if it is admitted, or its
    /// cancel; hands back any other message. `bytes` is what its frame
    /// weighed.
    pub(crate) fn take(
        &self,
        daemon: &Daemon,
        outbox: &Outbox,
        role: Role,
        msg: ClientMsg,
        bytes: usize,
    ) -> Option<ClientMsg> {
        let (id, service, scope, body) = match msg {
            ClientMsg::Query {
                id,
                service,
                scope,
                body,
            } => (id, service, scope, body),
            ClientMsg::Cancel { id } => {
                if let Some(cancel) = lock(&self.0).get(&id) {
                    cancel.set();
                }
                return None;
            }
            msg => return Some(msg),
        };
        // Not a reply: that would read as the running query's.
        if lock(&self.0).contains_key(&id) {
            let message = format!("query {id} is running already");
            reply(outbox, &DaemonMsg::Error { message });
            return None;
        }
        let failed = |error: String| {
            let outcome = Outcome::Failed { error };
            reply(outbox, &DaemonMsg::Reply { id, outcome });
            None
        };
        let Some(permit) = QueuePermit::query(bytes) else {
            return failed("daemon admission is full".into());
        };
        let registry = daemon.shared.registry.get();
        let Some(query) = registry.query(&service) else {
            return failed(format!("no query named {service}"));
        };
        let at = match scope {
            Scope::Daemon => At::Daemon,
            Scope::Workspace(dir) => At::Workspace(Arc::from(workspace_of(&dir, None))),
            Scope::Session(session) => match daemon.slot(session) {
                Some(slot) => At::Session(session, Arc::from(slot.header.workspace.as_path())),
                None => return failed(format!("no session {session}")),
            },
        };
        let cancel = Cancel::default();
        lock(&self.0).insert(id, cancel.clone());
        let asked = Asked {
            role,
            cancel: cancel.clone(),
            services: Arc::clone(&daemon.queried),
            registry,
        };
        let (running, outbox) = (Arc::clone(&self.0), outbox.clone());
        daemon.shared.rt.spawn(Box::pin(async move {
            let _permit = permit;
            let updates = Paced {
                outbox: &outbox,
                id,
            };
            // A query that panics fails, as a tool's call does.
            let answer = contain(query.answer(&body, &at, asked, &updates));
            let outcome = match cancel.until(answer).await {
                Some(Ok(output)) => Outcome::Done { output },
                Some(Err(error)) => Outcome::Failed { error },
                None => Outcome::Interrupted { output: None },
            };
            lock(&running).remove(&id);
            reply(&outbox, &DaemonMsg::Reply { id, outcome });
        }));
        None
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
