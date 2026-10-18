//! Drain-and-restart upgrades: once a newer build is staged, the first client
//! of another build makes this daemon finish its work and exit, so the next
//! connection starts `runtime/current`.
//!
//! Several builds share one home (the app's bundled `a`, a CLI from brew or
//! cargo), and the rules keep them from fighting:
//!
//! - Every entry point stages itself, and staging only ever moves `current`
//!   to a newer build (higher version, else later build time). So `current`
//!   names the newest build ever run here, and a spawned daemon runs it.
//! - A daemon whose build is `current` serves every client whose protocol it
//!   speaks, older builds included: draining for them would only start the
//!   same build again. An older client it cannot speak to gets a mismatch
//!   (and is told to run `runtime/current/a`), never a downgrade.
//! - A daemon that `current` moved past drains on the first client of
//!   another build, older or newer: the next daemon is the newest either
//!   way. Its own build's clients are served until then.
//!
//! Since `current` never moves back, a home drains at most once per newer
//! build staged, never back and forth. The app's "Install CLI" links the
//! CLI to `runtime/current/a`, so the two are the same build to begin with.

use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use protocol::{DaemonMsg, Frame, Versions};
use sys::Runtime;

use crate::lock;
use crate::outbox::Outbox;

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
    pub fn of(
        proto: impl Into<Versions>,
        client: &str,
        own: &str,
        current: Option<&str>,
        draining: bool,
    ) -> Self {
        if draining || (client != own && current.is_some_and(|c| c != own)) {
            Self::Drain
        } else if Versions::OURS.common(proto.into()).is_some() {
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
    conns: Mutex<Vec<(u64, Outbox)>>,
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

    pub fn verdict(&self, proto: impl Into<Versions>, client: &str) -> Verdict {
        // A `current` that cannot be started is no reason to stop serving.
        let current = self
            .runtime
            .current()
            .filter(|_| self.runtime.current_exe().is_file());
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
    pub fn register(&self, tx: Outbox) -> Registered<'_> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.conns).push((id, tx));
        Registered { life: self, id }
    }
}

pub struct Registered<'a> {
    life: &'a Lifecycle,
    id: u64,
}

impl Registered<'_> {
    /// The connection's id for its client, from 1: what its welcome and
    /// the origins of its mutations carry.
    pub fn client(&self) -> u32 {
        u32::try_from(self.id + 1).unwrap_or(u32::MAX)
    }
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        lock(&self.life.conns).retain(|(id, _)| *id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use super::Lifecycle;
    use super::Verdict::{self, Drain, Mismatch, Welcome};
    use protocol::PROTO;
    use sys::Runtime;

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

    /// A fake build of `a`, built `age` seconds after the epoch.
    fn build(dir: &Path, name: &str, age: u64) -> PathBuf {
        let exe = dir.join(name);
        fs::write(&exe, name).unwrap();
        let built = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + age);
        File::options()
            .write(true)
            .open(&exe)
            .unwrap()
            .set_modified(built)
            .unwrap();
        exe
    }

    /// Two builds in one home, staged in either order: whichever daemon
    /// runs first, the clients of both end on the newer one after at most
    /// one drain, and stay there.
    #[test]
    fn two_builds_drain_at_most_once_and_never_back() {
        for newer_first in [false, true] {
            let dir = std::env::temp_dir()
                .join(format!("a-upgrade-{newer_first}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let runtime = Runtime::new(dir.join("runtime"));
            let (old, new) = (build(&dir, "old", 1), build(&dir, "new", 2));
            let order = if newer_first {
                [&new, &old]
            } else {
                [&old, &new]
            };
            let first = runtime.stage(order[0]).unwrap().id;
            let spawned = first.clone();
            let second = runtime.stage(order[1]).unwrap().id;
            let (old, new) = if newer_first {
                (second, first)
            } else {
                (first, second)
            };
            assert_eq!(runtime.current().as_ref(), Some(&new));

            // The daemon the first client spawned ran what was current then.
            let mut daemon = Lifecycle::new(spawned, runtime.clone());
            let mut drains = 0;
            for client in [&old, &new].repeat(3) {
                match daemon.verdict(PROTO, client) {
                    Welcome => {}
                    Drain => {
                        drains += 1;
                        daemon.drain();
                        // Its successor runs `current`.
                        daemon = Lifecycle::new(runtime.current().unwrap(), runtime.clone());
                    }
                    Mismatch => panic!("{client} was refused"),
                }
            }
            assert_eq!(daemon.build, new);
            assert_eq!(drains, usize::from(!newer_first));
            // An older client the newest daemon cannot speak to is refused,
            // not served by a downgrade.
            assert_eq!(daemon.verdict(PROTO + 1, &old), Mismatch);
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_current_that_is_gone_is_no_reason_to_drain() {
        let dir = std::env::temp_dir().join(format!("a-upgrade-gone-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let runtime = Runtime::new(dir.join("runtime"));
        let old = runtime.stage(&build(&dir, "old", 1)).unwrap().id;
        let new = runtime.stage(&build(&dir, "new", 2)).unwrap().id;
        let daemon = Lifecycle::new(old.clone(), runtime.clone());
        assert_eq!(daemon.verdict(PROTO, &new), Drain);
        fs::remove_dir_all(dir.join("runtime").join(&new)).unwrap();
        let daemon = Lifecycle::new(old, runtime);
        assert_eq!(daemon.verdict(PROTO, &new), Welcome);
        fs::remove_dir_all(&dir).unwrap();
    }
}
