//! The I/O under a [`SessionClient`]: connections on their own threads,
//! their messages handed to a sink as [`Event`]s, and the client's
//! [`Io`] effects carried out. The sink decides where events go (a
//! channel, a lock around the client), and [`Driver::pump`] is the one
//! loop every app drives a session with.
//!
//! [`SessionClient`]: crate::SessionClient

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use protocol::{ClientId, ClientMsg, Environment};

use crate::{
    Attach, ConnId, Connection, Daemon, Effect, Endpoint, Error, Event, EventKind, Io, Notice,
    Show, Writer,
};

type Sink = dyn Fn(&Driver, Event) + Send + Sync;
type Origin = dyn Fn() -> io::Result<Environment> + Send + Sync;

/// What renders a session client's [`Show`] effects: an app's screen, a
/// web page, a feed to another language. Showing may ask the client for
/// more (a picker opening asks for the list), which comes back as effects.
pub trait Renderer {
    fn show(&mut self, shown: Vec<Show>) -> Vec<Effect>;
}

/// Shows nothing: for effects carried out where no one is watching, such
/// as the detach of a dropped session.
impl Renderer for () {
    fn show(&mut self, _: Vec<Show>) -> Vec<Effect> {
        Vec::new()
    }
}

/// The fixpoint every app runs: the client's I/O is carried out by `io`,
/// what it shows is rendered, and whatever rendering asks for goes round
/// again. A message that could not be sent is shown as a notice.
pub fn pump(
    mut io: impl FnMut(Io) -> Result<(), Error>,
    renderer: &mut impl Renderer,
    mut effects: Vec<Effect>,
) {
    while !effects.is_empty() {
        let mut shown = Vec::new();
        for effect in effects {
            match effect {
                Effect::Io(what) => {
                    if let Err(e) = io(what) {
                        shown.push(Show::Notice(Notice::NotSent(e)));
                    }
                }
                Effect::Show(show) => shown.push(show),
            }
        }
        if shown.is_empty() {
            return;
        }
        effects = renderer.show(shown);
    }
}

/// Runs one session client's connections; cheap to clone.
#[derive(Clone)]
pub struct Driver(Arc<Inner>);

struct Inner {
    daemon: Daemon,
    /// Who the session client is, on every connection it makes.
    client: ClientId,
    /// The newest connection asked for, and its write half once it is up.
    latest: AtomicU64,
    cancelled: AtomicU64,
    writer: Mutex<Option<(ConnId, Writer)>>,
    /// This client's environment, for `/env refresh`.
    origin: Box<Origin>,
    sink: Box<Sink>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Driver {
    /// A driver reaching `daemon`, handing what happens to `sink`.
    pub fn new(daemon: Daemon, sink: impl Fn(&Self, Event) + Send + Sync + 'static) -> Self {
        Self::with_origin(daemon, crate::capture, sink)
    }

    /// [`Driver::new`], refreshing the environment from `origin` rather
    /// than this process's.
    pub fn with_origin(
        daemon: Daemon,
        origin: impl Fn() -> io::Result<Environment> + Send + Sync + 'static,
        sink: impl Fn(&Self, Event) + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(Inner {
            daemon,
            client: ClientId::new(),
            latest: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            writer: Mutex::new(None),
            origin: Box::new(origin),
            sink: Box::new(sink),
        }))
    }

    pub fn daemon(&self) -> &Daemon {
        &self.0.daemon
    }

    /// [`pump`] with this driver's I/O.
    pub fn pump(&self, renderer: &mut impl Renderer, effects: Vec<Effect>) {
        pump(|io| self.io(io), renderer, effects);
    }

    /// Carries out `io`.
    pub fn io(&self, io: Io) -> Result<(), Error> {
        match io {
            Io::Connect {
                conn,
                attach,
                restart,
            } => {
                self.0.latest.fetch_max(conn.0, Ordering::AcqRel);
                let driver = self.clone();
                thread::spawn(move || driver.connect(conn, &attach, restart));
                Ok(())
            }
            Io::Send(msg) => self.write(&msg),
            Io::SendEnvironment => {
                let env = (self.0.origin)()?;
                self.write(&ClientMsg::Environment(env))
            }
            Io::Hangup => {
                let mut slot = lock(&self.0.writer);
                self.0
                    .cancelled
                    .fetch_max(self.0.latest.load(Ordering::Acquire), Ordering::AcqRel);
                slot.take();
                Ok(())
            }
        }
    }

    fn write(&self, msg: &ClientMsg) -> Result<(), Error> {
        match &mut *lock(&self.0.writer) {
            Some((_, writer)) => Ok(writer.send(msg)?),
            None => Err(Error::Closed),
        }
    }

    /// Opens connection `conn` on this thread, attaching as `attach` says
    /// (after replacing the daemon when `restart`), then reads it on its
    /// own. A daemon that does not answer is diagnosed.
    pub fn connect(&self, conn: ConnId, attach: &Attach, restart: bool) {
        self.0.latest.fetch_max(conn.0, Ordering::AcqRel);
        let sink = |kind| (self.0.sink)(self, Event { conn, kind });
        let on_drain = || sink(EventKind::Waiting);
        let first = |c: &Connection| Ok(vec![c.attach(attach)?]);
        let daemon = &self.0.daemon;
        let client = Some(self.0.client);
        let made = match &daemon.endpoint {
            Endpoint::Local(home) if restart => crate::restart(home, &daemon.staged, client)
                .and_then(|mut c| {
                    c.send_all(&first(&c)?)?;
                    Ok(c)
                }),
            _ => daemon.connect_as(client, &on_drain, &first),
        };
        let Connection {
            mut reader,
            mut writer,
            remote,
            welcome,
            commands,
        } = match made {
            Ok(c) => c,
            Err(e) => {
                return sink(match &daemon.endpoint {
                    Endpoint::Local(home) if e == Error::NotResponding => {
                        EventKind::Wedged(crate::diagnose(home))
                    }
                    _ => EventKind::Failed(e),
                });
            }
        };
        {
            let mut slot = lock(&self.0.writer);
            // Replaced while it was being made.
            if conn.0 < self.0.latest.load(Ordering::Acquire)
                || conn.0 <= self.0.cancelled.load(Ordering::Acquire)
            {
                let _ = writer.send(&ClientMsg::Detach);
                return;
            }
            *slot = Some((conn, writer));
        }
        sink(EventKind::Connected {
            welcome,
            remote: remote.is_some(),
            commands,
        });
        let driver = self.clone();
        thread::spawn(move || {
            let why = loop {
                match reader.recv() {
                    Ok(Some(msg)) => (driver.0.sink)(
                        &driver,
                        Event {
                            conn,
                            kind: EventKind::Msg(msg),
                        },
                    ),
                    Ok(None) => break Error::Closed,
                    Err(e) => break e.into(),
                }
            };
            {
                let mut slot = lock(&driver.0.writer);
                if slot.as_ref().is_some_and(|(c, _)| *c == conn) {
                    slot.take();
                }
            }
            (driver.0.sink)(
                &driver,
                Event {
                    conn,
                    kind: EventKind::Closed(why),
                },
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hung_up_connection_never_installs_after_a_late_hello() {
        use protocol::{Caps, DaemonMsg, Target, read_frame, write_frame};
        use std::sync::mpsc;
        use std::time::Duration;
        use sys::Home;

        let root = std::env::temp_dir().join(format!("a-late-hello-{:?}", ClientId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let home = Home::from(root.join("home"));
        let exe = root.join("a");
        std::fs::write(&exe, "never run").unwrap();
        let staged = home.runtime().stage(&exe).unwrap();
        let listener = home.bind().unwrap();
        let (tx, rx) = mpsc::channel();
        let driver = Driver::new(Daemon::local(home.clone(), staged), move |_, event| {
            tx.send(event).unwrap();
        });
        driver
            .io(Io::Connect {
                conn: ConnId(1),
                attach: Attach::new(Target::New, crate::Env::Keep),
                restart: false,
            })
            .unwrap();
        let mut peer = listener.accept().unwrap();
        assert!(matches!(
            read_frame::<ClientMsg>(&mut peer).unwrap(),
            Some(ClientMsg::Hello { .. })
        ));
        assert!(matches!(
            read_frame::<ClientMsg>(&mut peer).unwrap(),
            Some(ClientMsg::Attach { .. })
        ));
        driver.io(Io::Hangup).unwrap();
        write_frame(
            &mut peer,
            &DaemonMsg::Welcome {
                proto: protocol::PROTO,
                build: "test".into(),
                caps: Caps::ALL,
                commands: Vec::new(),
            },
        )
        .unwrap();
        // Join the opener by waiting for its sole driver reference to go away.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&driver.0) > 1 && rx.try_recv().is_err() {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        let installed = lock(&driver.0.writer).is_some();
        drop(peer);
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
        assert!(!installed, "a cancelled opener installed its writer");
    }

    /// A renderer that asks for one more thing the first time it is shown
    /// anything.
    struct Again(bool);

    impl Renderer for Again {
        fn show(&mut self, _: Vec<Show>) -> Vec<Effect> {
            if std::mem::replace(&mut self.0, true) {
                Vec::new()
            } else {
                vec![Effect::Io(Io::Hangup), Effect::Show(Show::Link)]
            }
        }
    }

    #[test]
    fn the_pump_runs_until_the_renderer_asks_for_nothing_more() {
        let mut done = Vec::new();
        let mut renderer = Again(false);
        pump(
            |io| {
                done.push(io);
                Ok(())
            },
            &mut renderer,
            vec![Effect::Show(Show::Sessions)],
        );
        assert!(renderer.0 && matches!(done[..], [Io::Hangup]));
        // Nothing to render, nothing to pump; a failed send is rendered.
        let mut renderer = Again(false);
        pump(|_| Ok(()), &mut renderer, vec![Effect::Io(Io::Hangup)]);
        assert!(!renderer.0);
        pump(
            |_| Err(Error::Closed),
            &mut renderer,
            vec![Effect::Io(Io::Hangup)],
        );
        assert!(renderer.0);
    }
}
