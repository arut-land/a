//! One writer per session: each appender holds a lease whose epoch every
//! entry it writes carries, and a newer lease fences the older one off.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use protocol::{Entry, SessionId};

/// Grants the leases on sessions' logs.
pub trait LeaseAuthority: Send + Sync {
    /// A lease on session `id` newer than every one granted before it and
    /// than `floor`, the newest epoch the log holds. It supersedes them all.
    fn acquire(&self, id: SessionId, floor: u32) -> Lease;
}

/// One writer's hold on a session, until a newer lease supersedes it.
#[derive(Clone, Debug)]
pub struct Lease {
    pub epoch: u32,
    newest: Arc<AtomicU32>,
}

impl Lease {
    pub fn is_current(&self) -> bool {
        self.newest.load(Ordering::SeqCst) == self.epoch
    }

    /// Refuses `entries` unless they carry this lease's epoch and it is
    /// still the newest.
    pub fn admit(&self, entries: &[Entry]) -> io::Result<()> {
        if !self.is_current() {
            return Err(stale(format!(
                "the lease of epoch {} was superseded",
                self.epoch
            )));
        }
        match entries.iter().find(|e| e.epoch != self.epoch) {
            Some(e) => Err(stale(format!(
                "entry {} carries epoch {}, not the lease's {}",
                e.seq, e.epoch, self.epoch
            ))),
            None => Ok(()),
        }
    }
}

/// Why an append was refused: its writer's lease is not current.
fn stale(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::ResourceBusy, why)
}

/// Leases granted in this process. Enough while one daemon, holding the
/// daemon lock, is the only process writing its sessions; leases that span
/// machines come with migration.
#[derive(Default)]
pub struct Local(Mutex<HashMap<SessionId, Arc<AtomicU32>>>);

impl LeaseAuthority for Local {
    fn acquire(&self, id: SessionId, floor: u32) -> Lease {
        let newest = Arc::clone(
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(id)
                .or_default(),
        );
        let next = |epoch: u32| epoch.max(floor).saturating_add(1);
        let last = newest
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
                Some(next(epoch))
            })
            .unwrap_or_else(|epoch| epoch);
        Lease {
            epoch: next(last),
            newest,
        }
    }
}
