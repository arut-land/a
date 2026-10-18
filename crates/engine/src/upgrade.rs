//! Drain-and-restart upgrades: once a newer build is staged, the first client
//! of another build makes this daemon finish its work and exit, so the next
//! connection starts `runtime/current`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use protocol::{DaemonMsg, Frame, PROTO};
use sys::Runtime;

use crate::lock;

/// How the daemon answers a hello.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Welcome,
    Mismatch,
    /// Make way for a newer build.
    Drain,
}

impl Verdict {
    /// `current` is what `runtime/current` names now. It moving away from
    /// `own` means a newer build was staged (staging never downgrades), and a
    /// client of another build is the cue to hand over. Clients of this very
    /// build keep being served until then, whatever `current` says.
    pub fn of(proto: u32, client: &str, own: &str, current: Option<&str>, draining: bool) -> Self {
        if draining || (client != own && current.is_some_and(|c| c != own)) {
            Self::Drain
        } else if proto == PROTO {
            Self::Welcome
        } else {
            Self::Mismatch
        }
    }
}

/// This daemon's build and the state of its drain.
pub struct Lifecycle {
    pub build: String,
    runtime: Runtime,
    /// When the drain began.
    draining: OnceLock<Instant>,
    /// Every open connection's outbox, told when a drain begins.
    conns: Mutex<Vec<(u64, Sender<Frame>)>>,
    next: AtomicU64,
}

impl Lifecycle {
    pub fn new(build: String, runtime: Runtime) -> Self {
        Self {
            build,
            runtime,
            draining: OnceLock::new(),
            conns: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        }
    }

    pub fn verdict(&self, proto: u32, client: &str) -> Verdict {
        let current = self.runtime.current();
        Verdict::of(
            proto,
            client,
            &self.build,
            current.as_deref(),
            self.draining().is_some(),
        )
    }

    pub fn draining(&self) -> Option<Instant> {
        self.draining.get().copied()
    }

    /// Starts the drain once, telling every open connection.
    pub fn drain(&self) {
        if self.draining.set(Instant::now()).is_ok() {
            eprintln!("a newer build is staged; draining");
            if let Ok(draining) = Frame::new(&DaemonMsg::Draining) {
                for (_, tx) in &*lock(&self.conns) {
                    let _ = tx.send(draining.clone());
                }
            }
        }
    }

    /// Registers a connection's outbox until the guard drops.
    pub fn register(&self, tx: Sender<Frame>) -> Registered<'_> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.conns).push((id, tx));
        Registered { life: self, id }
    }
}

pub struct Registered<'a> {
    life: &'a Lifecycle,
    id: u64,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        lock(&self.life.conns).retain(|(id, _)| *id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::Verdict::{self, Drain, Mismatch, Welcome};
    use protocol::PROTO;

    const OWN: &str = "0.1.0+aa";
    const NEW: &str = "0.2.0+bb";

    fn of(proto: u32, client: &str, current: Option<&str>) -> Verdict {
        Verdict::of(proto, client, OWN, current, false)
    }

    #[test]
    fn a_client_of_another_build_drains_once_current_moved_on() {
        // Nothing newer staged: serve old and new clients alike.
        assert_eq!(of(PROTO, NEW, Some(OWN)), Welcome);
        assert_eq!(of(PROTO, NEW, None), Welcome);
        // Newer build staged.
        assert_eq!(of(PROTO, NEW, Some(NEW)), Drain);
        assert_eq!(of(PROTO + 1, NEW, Some(NEW)), Drain);
        // An old client after a newer one was staged also gets the new daemon.
        assert_eq!(of(PROTO, "0.0.9+cc", Some(NEW)), Drain);
        // Clients of this build are served until a drain begins.
        assert_eq!(of(PROTO, OWN, Some(NEW)), Welcome);
        assert_eq!(Verdict::of(PROTO, OWN, OWN, Some(OWN), true), Drain);
        assert_eq!(of(PROTO + 1, OWN, Some(OWN)), Mismatch);
    }
}
