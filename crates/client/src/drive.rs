//! The I/O under a [`SessionClient`]: connections on their own threads,
//! their messages handed to a sink as [`Event`]s, and the client's
//! [`Effect`]s carried out. The sink decides where events go (a channel,
//! a lock around the client), so every app drives a session the same way.
//!
//! [`SessionClient`]: crate::SessionClient

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use protocol::{ClientMsg, Environment};
use sys::Staged;

use crate::{Attach, Connection, Effect, Endpoint, Event, Writer};

type Sink = dyn Fn(&Driver, Event) + Send + Sync;
type Origin = dyn Fn() -> io::Result<Environment> + Send + Sync;

/// Runs one session client's connections; cheap to clone.
#[derive(Clone)]
pub struct Driver(Arc<Inner>);

struct Inner {
    endpoint: Endpoint,
    staged: Staged,
    /// The newest connection asked for, and its write half once it is up.
    latest: AtomicU64,
    writer: Mutex<Option<(u64, Writer)>>,
    /// This client's environment, for `/env refresh`.
    origin: Box<Origin>,
    sink: Box<Sink>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Driver {
    /// A driver reaching the daemon at `endpoint` as build `staged`,
    /// handing what happens to `sink`.
    pub fn new(
        endpoint: Endpoint,
        staged: Staged,
        sink: impl Fn(&Self, Event) + Send + Sync + 'static,
    ) -> Self {
        Self::with_origin(endpoint, staged, crate::capture, sink)
    }

    /// [`Driver::new`], refreshing the environment from `origin` rather
    /// than this process's.
    pub fn with_origin(
        endpoint: Endpoint,
        staged: Staged,
        origin: impl Fn() -> io::Result<Environment> + Send + Sync + 'static,
        sink: impl Fn(&Self, Event) + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Inner {
            endpoint,
            staged,
            latest: AtomicU64::new(0),
            writer: Mutex::new(None),
            origin: Box::new(origin),
            sink: Box::new(sink),
        }))
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.0.endpoint
    }

    /// Carries out `effect` if it is the driver's, handing back the app's
    /// effects and, for a message it could not send, a notice saying so.
    pub fn apply(&self, effect: Effect) -> Option<Effect> {
        let sent = match effect {
            Effect::Connect {
                id,
                attach,
                restart,
            } => {
                self.0.latest.fetch_max(id, Ordering::AcqRel);
                let driver = self.clone();
                thread::spawn(move || driver.connect(id, &attach, restart));
                Ok(())
            }
            Effect::Send(msg) => self.write(&msg),
            Effect::SendEnvironment => {
                (self.0.origin)().and_then(|env| self.write(&ClientMsg::Environment(env)))
            }
            Effect::Hangup => {
                lock(&self.0.writer).take();
                Ok(())
            }
            app => return Some(app),
        };
        sent.err().map(|e| Effect::Notice(format!("not sent: {e}")))
    }

    fn write(&self, msg: &ClientMsg) -> io::Result<()> {
        match &mut *lock(&self.0.writer) {
            Some((_, writer)) => writer.send(msg),
            None => Err(io::Error::new(io::ErrorKind::NotConnected, "not connected")),
        }
    }

    /// Opens connection `id` on this thread, attaching as `attach` says
    /// (after replacing the daemon when `restart`), then reads it on its
    /// own. A daemon that does not answer is diagnosed.
    pub fn connect(&self, id: u64, attach: &Attach, restart: bool) {
        self.0.latest.fetch_max(id, Ordering::AcqRel);
        let sink = |event| (self.0.sink)(self, event);
        let on_drain = || sink(Event::Waiting { id });
        let first = |conn: &Connection| Ok(vec![conn.attach(attach)?]);
        let made = match &self.0.endpoint {
            Endpoint::Local(home) if restart => {
                crate::restart(home, &self.0.staged).and_then(|mut conn| {
                    conn.send_all(&first(&conn)?)?;
                    Ok(conn)
                })
            }
            endpoint => crate::connect_with(endpoint, &self.0.staged, &on_drain, &first),
        };
        let Connection {
            mut reader,
            mut writer,
            remote,
            welcome,
            commands,
        } = match made {
            Ok(conn) => conn,
            Err(e) => {
                return sink(match &self.0.endpoint {
                    Endpoint::Local(home) if e.kind() == io::ErrorKind::TimedOut => Event::Wedged {
                        id,
                        diagnosis: crate::diagnose(home),
                    },
                    _ => Event::Failed {
                        id,
                        why: e.to_string(),
                    },
                });
            }
        };
        {
            let mut slot = lock(&self.0.writer);
            // Replaced while it was being made.
            if id < self.0.latest.load(Ordering::Acquire) {
                let _ = writer.send(&ClientMsg::Detach);
                return;
            }
            *slot = Some((id, writer));
        }
        sink(Event::Connected {
            id,
            welcome,
            remote: remote.is_some(),
            commands,
        });
        let driver = self.clone();
        thread::spawn(move || {
            let why = loop {
                match reader.recv() {
                    Ok(Some(msg)) => (driver.0.sink)(&driver, Event::Msg { id, msg }),
                    Ok(None) => break "the daemon closed the connection".to_owned(),
                    Err(e) => break e.to_string(),
                }
            };
            {
                let mut slot = lock(&driver.0.writer);
                if slot.as_ref().is_some_and(|(i, _)| *i == id) {
                    slot.take();
                }
            }
            (driver.0.sink)(&driver, Event::Closed { id, why });
        });
    }
}
