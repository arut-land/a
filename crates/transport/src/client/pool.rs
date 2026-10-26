//! Idle keep-alive connections, ureq's limits: ten in all, three per
//! target, none older than fifteen seconds. Expiry is checked when the pool
//! is used, never on a timer, so an idle process stays asleep.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::conn::{Conn, Target};

const MAX_IDLE: usize = 10;
const MAX_IDLE_PER_TARGET: usize = 3;
const MAX_IDLE_AGE: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(super) struct Pool(Mutex<Vec<Idle>>);

struct Idle {
    target: Target,
    conn: Conn,
    since: Instant,
}

impl Pool {
    /// The most recently returned connection to `target` that is still
    /// open.
    pub fn take(&self, target: &Target, now: Instant) -> Option<Conn> {
        let mut idle = self.lock(now);
        while let Some(at) = idle.iter().rposition(|idle| idle.target == *target) {
            let conn = idle.remove(at).conn;
            if conn.is_reusable() {
                return Some(conn);
            }
        }
        None
    }

    pub fn put(&self, target: Target, conn: Conn, now: Instant) {
        let mut idle = self.lock(now);
        if idle.iter().filter(|idle| idle.target == target).count() >= MAX_IDLE_PER_TARGET {
            let oldest = idle
                .iter()
                .position(|idle| idle.target == target)
                .expect("a connection to the target");
            idle.remove(oldest);
        }
        if idle.len() >= MAX_IDLE {
            idle.remove(0);
        }
        idle.push(Idle {
            target,
            conn,
            since: now,
        });
    }

    /// The idle list, oldest first, without what has expired.
    fn lock(&self, now: Instant) -> std::sync::MutexGuard<'_, Vec<Idle>> {
        let mut idle = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        idle.retain(|idle| now.saturating_duration_since(idle.since) < MAX_IDLE_AGE);
        idle
    }
}
