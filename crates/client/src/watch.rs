//! The daemon's session list as it changes, pushed over one connection
//! that is made again whenever the daemon goes (an upgrade, a restart).

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use protocol::{ClientMsg, DaemonMsg, SessionSummary};

use crate::{Connection, Daemon, Error, Writer};

/// How long a watch waits to try again after failing to reach the daemon.
const REWATCH: Duration = Duration::from_secs(1);

/// Watches the session list until dropped.
pub struct SessionWatch {
    control: Arc<Control>,
}

impl SessionWatch {
    /// Hands `sink` the list (newest first) now and after each change,
    /// starting the daemon if needed and again after it restarts; while
    /// it cannot be reached, why.
    pub fn start(
        daemon: Daemon,
        sink: impl Fn(Result<&[SessionSummary], &Error>) + Send + 'static,
    ) -> Self {
        let control = Arc::new(Control::default());
        let stop = Arc::clone(&control);
        thread::spawn(move || {
            while !stop.lock().cancelled {
                let connected = daemon
                    .connect_as(None, &|| {}, &|_| Ok(vec![ClientMsg::WatchSessions]))
                    .map(|Connection { reader, writer, .. }| {
                        stop.install(writer).then_some(reader)
                    });
                let met = match connected {
                    Ok(Some(mut reader)) => {
                        loop {
                            match reader.recv() {
                                Ok(Some(DaemonMsg::Sessions { sessions })) => sink(Ok(&sessions)),
                                Ok(Some(_)) => {}
                                Ok(None) | Err(_) => break,
                            }
                        }
                        Ok(())
                    }
                    Ok(None) => return,
                    Err(e) => Err(e),
                };
                stop.lock().writer.take();
                if stop.lock().cancelled {
                    return;
                }
                // A closed connection can reattach immediately; failed connects back off.
                if let Err(e) = met {
                    sink(Err(&e));
                    if !stop.backoff(stop.lock(), REWATCH) {
                        return;
                    }
                }
            }
        });
        Self { control }
    }
}

#[derive(Default)]
struct State {
    cancelled: bool,
    writer: Option<Writer>,
}

#[derive(Default)]
struct Control {
    state: Mutex<State>,
    changed: Condvar,
}

impl Control {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn install(&self, writer: Writer) -> bool {
        let mut state = self.lock();
        if state.cancelled {
            return false;
        }
        state.writer = Some(writer);
        true
    }

    fn backoff(&self, state: std::sync::MutexGuard<'_, State>, delay: Duration) -> bool {
        let (state, _) = self
            .changed
            .wait_timeout_while(state, delay, |state| !state.cancelled)
            .unwrap_or_else(PoisonError::into_inner);
        !state.cancelled
    }

    fn cancel(&self) {
        let writer = {
            let mut state = self.lock();
            state.cancelled = true;
            self.changed.notify_all();
            state.writer.take()
        };
        // Dropping the writer shuts down the local reader even if the peer is stalled.
        drop(writer);
    }
}

impl Drop for SessionWatch {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::mpsc;

    struct Transport(mpsc::Sender<()>);

    impl Write for Transport {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl crate::WriteTransport for Transport {
        fn shutdown(&mut self) {
            self.0.send(()).unwrap();
        }
    }

    #[test]
    fn cancel_during_connect_rejects_late_installation() {
        let control = Arc::new(Control::default());
        let watch = SessionWatch {
            control: Arc::clone(&control),
        };
        let (connected, connect) = mpsc::channel();
        let (closed, shutdown) = mpsc::channel();
        let worker = thread::spawn(move || {
            let writer = connect.recv().unwrap();
            assert!(!control.install(writer));
            assert!(control.lock().writer.is_none());
        });
        drop(watch);
        connected.send(Writer(Box::new(Transport(closed)))).unwrap();
        worker.join().unwrap();
        shutdown.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn cancel_interrupts_reconnect_backoff() {
        let control = Arc::new(Control::default());
        let watch = SessionWatch {
            control: Arc::clone(&control),
        };
        let (waiting, ready) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let worker = thread::spawn(move || {
            // Cancellation must acquire the mutex after the waiter releases it.
            let state = control.lock();
            waiting.send(()).unwrap();
            finished
                .send(!control.backoff(state, Duration::from_secs(60)))
                .unwrap();
        });
        ready.recv().unwrap();
        drop(watch);
        assert!(done.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
    }
}
