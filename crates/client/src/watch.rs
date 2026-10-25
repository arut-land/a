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
    /// and why each member that cannot list cannot, whenever either
    /// changes. A member that fails lists nothing until it is reached
    /// again; the others' lists stay.
    pub fn start(
        rt: &rt::Rt,
        daemons: &Daemons,
        sink: impl Fn(&[Listed], &[Problem]) + Send + Sync + 'static,
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
                    EventKind::Failed(error) => lists.failed(at, &on, error),
                    EventKind::Wedged(_) => lists.failed(at, &on, Error::NotResponding),
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

/// Why a member lists nothing now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The member's name.
    pub on: String,
    pub error: Error,
}

type Sink = dyn Fn(&[Listed], &[Problem]) + Send + Sync;

struct Lists {
    members: Mutex<Vec<Listing>>,
    sink: Box<Sink>,
}

/// One member's latest list, the machine its connection reached, and why
/// it cannot list, while it cannot.
#[derive(Default)]
struct Listing {
    machine: Option<MachineId>,
    sessions: Vec<Listed>,
    problem: Option<Problem>,
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
        members[at].problem = None;
        members[at].sessions = sessions
            .into_iter()
            .map(|summary| Listed {
                on: on.to_owned(),
                machine,
                summary,
            })
            .collect();
        self.tell(&members);
    }

    fn failed(&self, at: usize, on: &str, error: Error) {
        let mut members = self.lock();
        members[at] = Listing {
            problem: Some(Problem {
                on: on.to_owned(),
                error,
            }),
            ..Listing::default()
        };
        self.tell(&members);
    }

    /// Calls the sink under the lock, so that what it is told stays in order.
    fn tell(&self, members: &[Listing]) {
        let listed: Vec<Listed> = members
            .iter()
            .flat_map(|m| m.sessions.iter().cloned())
            .collect();
        let problems: Vec<Problem> = members.iter().filter_map(|m| m.problem.clone()).collect();
        (self.sink)(&listed, &problems);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Listing>> {
        self.members.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
