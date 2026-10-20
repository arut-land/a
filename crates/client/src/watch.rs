//! The daemon's session list as it changes, pushed over one connection
//! that is made again whenever the daemon goes (an upgrade, a restart).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use protocol::{ClientMsg, DaemonMsg, SessionSummary};

use crate::{Connection, Daemon, Error, Writer};

/// How long a watch waits to try again after failing to reach the daemon.
const REWATCH: Duration = Duration::from_secs(1);

/// Watches the session list until dropped.
pub struct SessionWatch {
    /// The open connection's write half, for the detach that ends it.
    writer: Arc<Mutex<Option<Writer>>>,
    done: Arc<AtomicBool>,
}

impl SessionWatch {
    /// Hands `sink` the list (newest first) now and after each change,
    /// starting the daemon if needed and again after it restarts; while
    /// it cannot be reached, why.
    pub fn start(
        daemon: Daemon,
        sink: impl Fn(Result<&[SessionSummary], &Error>) + Send + 'static,
    ) -> Self {
        let writer: Arc<Mutex<Option<Writer>>> = Arc::default();
        let done: Arc<AtomicBool> = Arc::default();
        let (slot, stop) = (Arc::clone(&writer), Arc::clone(&done));
        thread::spawn(move || {
            let lock = || slot.lock().unwrap_or_else(PoisonError::into_inner);
            while !stop.load(Ordering::Acquire) {
                let met = match daemon.connect(&|| {}).and_then(|mut c| {
                    c.watch_sessions()?;
                    Ok(c)
                }) {
                    Ok(Connection {
                        mut reader, writer, ..
                    }) => {
                        *lock() = Some(writer);
                        loop {
                            match reader.recv() {
                                Ok(Some(DaemonMsg::Sessions { sessions })) => sink(Ok(&sessions)),
                                Ok(Some(_)) => {}
                                Ok(None) | Err(_) => break,
                            }
                        }
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                lock().take();
                if stop.load(Ordering::Acquire) {
                    return;
                }
                // A daemon that went (an upgrade, a restart) is followed at
                // once to the next one; only one that cannot be reached is
                // reported, and tried again later.
                if let Err(e) = met {
                    sink(Err(&e));
                    thread::sleep(REWATCH);
                }
            }
        });
        Self { writer, done }
    }
}

/// The daemon is told, and the reader then ends.
impl Drop for SessionWatch {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mut writer) = writer {
            let _ = writer.send(&ClientMsg::Detach);
        }
    }
}
