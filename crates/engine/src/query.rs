//! Queries a connection asks the extensions, each answered on a task of
//! its own and never logged: the session's log is not touched, and the
//! connection goes on reading while one runs. Every query admitted ends
//! with one [`DaemonMsg::Reply`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ext::Cancel;
use ext::host::{Registry, SessionServices};
use ext::query::{Asked, At};
use protocol::{DaemonMsg, Json, Outcome, QueryInfo, Role, Scope};

use crate::driver::contain;
use crate::outbox::{Outbox, QueuePermit};
use crate::server::{reply, workspace_of};
use crate::{Daemon, lock};

/// What a query is answered with.
pub(crate) struct Ask {
    pub id: u64,
    pub service: String,
    pub scope: Scope,
    pub body: Json,
    /// What the query frame weighed, as admission counts it.
    pub bytes: usize,
}

/// A connection's running queries, by the client's ids: cancelled as the
/// client asks or, all of them, as it hangs up.
#[derive(Default)]
pub(crate) struct Queries(Arc<Mutex<BTreeMap<u64, Cancel>>>);

impl Queries {
    /// Answers `ask` on its own task, if it is admitted.
    pub(crate) fn ask(&self, daemon: &Daemon, outbox: &Outbox, role: Role, ask: Ask) {
        let Ask {
            id,
            service,
            scope,
            body,
            bytes,
        } = ask;
        let failed = |error: String| {
            let outcome = Outcome::Failed { error };
            reply(outbox, &DaemonMsg::Reply { id, outcome });
        };
        if lock(&self.0).contains_key(&id) {
            return failed(format!("query {id} is running already"));
        }
        let Some(permit) = QueuePermit::acquire(bytes, false) else {
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
            services: Arc::new(
                SessionServices::default().with::<dyn rt::Runtime>(Arc::clone(&daemon.shared.rt)),
            ),
            registry,
        };
        let (running, outbox) = (Arc::clone(&self.0), outbox.clone());
        daemon.shared.rt.spawn(Box::pin(async move {
            let _permit = permit;
            let update = |body| reply(&outbox, &DaemonMsg::Update { id, body });
            // A query that panics fails, as a tool's call does.
            let answer = contain(query.answer(&body, &at, asked, &update));
            let outcome = match cancel.until(answer).await {
                Some(Ok(output)) => Outcome::Done { output },
                Some(Err(error)) => Outcome::Failed { error },
                None => Outcome::Interrupted { output: None },
            };
            lock(&running).remove(&id);
            reply(&outbox, &DaemonMsg::Reply { id, outcome });
        }));
    }

    /// Stops query `id`, if it runs: its reply says it was cancelled.
    pub(crate) fn cancel(&self, id: u64) {
        if let Some(cancel) = lock(&self.0).get(&id) {
            cancel.set();
        }
    }
}

impl Drop for Queries {
    fn drop(&mut self) {
        lock(&self.0).values().for_each(Cancel::set);
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
