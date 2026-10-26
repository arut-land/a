//! One writer per session: each appender holds a lease, whose epoch every
//! entry it writes carries. Within a daemon a stale writer cannot exist:
//! the [`Writer`](super::Writer) is moved, never shared, and a session is
//! loaded again only once its last writer is closed. Across hosts (M3) an
//! authority that spans them grants the epochs.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use protocol::SessionId;

/// Leases granted in this process: enough while one daemon, holding the
/// daemon lock, is the only process writing its sessions.
#[derive(Default)]
pub struct Local(Mutex<HashMap<SessionId, u32>>);

impl Local {
    /// Grants an epoch newer than every previous lease and the log’s `floor`.
    pub fn acquire(&self, id: SessionId, floor: u32) -> u32 {
        let mut granted = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let newest = granted.entry(id).or_default();
        *newest = (*newest).max(floor).saturating_add(1);
        *newest
    }
}
