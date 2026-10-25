//! The session lists of a set of daemons, each over a cancellable
//! runtime-owned connection of its own, merged in the set's order.

use std::sync::{Arc, Mutex, PoisonError};

use crate::{Attach, ClientHandle, ConnId, Daemons, Driver, Env, Error, EventKind, Io, Listed};
use protocol::MachineId;

/// Dropping the watch revokes connection attempts, reads and reconnect backoff.
pub struct SessionWatch {
    _handles: Vec<ClientHandle>,
}

impl SessionWatch {
    /// Calls `sink` with every member's sessions, the first member's first,
    /// whenever one's list changes. A member that fails is told as the
    /// error, and lists nothing until it is reached again.
    pub fn start(
        rt: &rt::Rt,
        daemons: &Daemons,
        sink: impl Fn(Result<&[Listed], &Error>) + Send + Sync + 'static,
    ) -> Self {
        let lists = Arc::new(Lists {
            members: Mutex::new(daemons.iter().map(|_| Listing::default()).collect()),
            sink: Box::new(sink),
        });
        let handles = daemons
            .iter()
            .enumerate()
            .map(|(at, member)| {
                let (lists, on) = (Arc::clone(&lists), member.name.clone());
                let mut driver = Driver::new(move |_, event| match event.kind {
                    EventKind::Connected { machine, .. } => lists.connected(at, machine),
                    EventKind::Msg(protocol::DaemonMsg::Sessions { sessions }) => {
                        lists.listed(at, &on, sessions);
                    }
                    EventKind::Failed(error) => lists.failed(at, &error),
                    EventKind::Wedged(_) => lists.failed(at, &Error::NotResponding),
                    _ => {}
                });
                driver.watch = true;
                let handle = driver.start(rt.clone(), member.daemon.clone());
                let _ = handle.io(Io::Connect {
                    conn: ConnId(1),
                    attach: Attach::new(protocol::Target::New, Env::Keep),
                    restart: false,
                });
                handle
            })
            .collect();
        Self { _handles: handles }
    }
}

type Sink = dyn Fn(Result<&[Listed], &Error>) + Send + Sync;

struct Lists {
    members: Mutex<Vec<Listing>>,
    sink: Box<Sink>,
}

/// One member's latest list, and the machine its connection reached.
#[derive(Default)]
struct Listing {
    machine: Option<MachineId>,
    sessions: Vec<Listed>,
}

impl Lists {
    fn connected(&self, at: usize, machine: MachineId) {
        self.lock()[at].machine = Some(machine);
    }

    fn listed(&self, at: usize, on: &str, sessions: Vec<protocol::SessionSummary>) {
        let mut members = self.lock();
        let Some(machine) = members[at].machine else {
            return;
        };
        members[at].sessions = sessions
            .into_iter()
            .map(|summary| Listed {
                on: on.to_owned(),
                machine,
                summary,
            })
            .collect();
        let merged: Vec<Listed> = members
            .iter()
            .flat_map(|m| m.sessions.iter().cloned())
            .collect();
        (self.sink)(Ok(&merged));
    }

    fn failed(&self, at: usize, error: &Error) {
        let mut members = self.lock();
        members[at] = Listing::default();
        (self.sink)(Err(error));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Listing>> {
        self.members.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
