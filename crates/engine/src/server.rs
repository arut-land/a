//! Connections, as tasks: a reader that takes a client's frames as they
//! arrive and a writer that follows the client's outbox, a cursor into the
//! session's frames. Both wait on the socket's readiness through the
//! reactor, so a connection costs no thread, and a client that stops
//! reading costs only its cursor.

use std::future::{Future, poll_fn};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use protocol::{
    Author, Caps, ClientDecl, ClientId, ClientMsg, ClientSeq, DaemonMsg, Decode, Environment,
    Frame, HomeSettings, Mode, Origin, PROTO, Rejection, Role, SettingsChange, Versions,
};
use rt::{Async, Reactor};
use sys::{Listener, Stream};

use crate::driver::{self, Attachment, Request};
use crate::outbox::Outbox;
use crate::upgrade::{Lifecycle, Verdict};
use crate::{Daemon, config, lock, session_event};

/// How long a client refused at its hello has to read the answer and hang
/// up before the daemon does.
const REFUSED_GRACE: Duration = Duration::from_secs(2);

/// How many bytes of ready frames go to a client in one write.
const WRITE_BATCH: usize = 64 << 10;

/// Serves every client that connects, each by tasks of its own.
pub(crate) async fn accept(
    listener: &Async<&Listener>,
    daemon: &Arc<Daemon>,
    life: &Arc<Lifecycle>,
) -> io::Result<()> {
    loop {
        let stream = listener.read_with(|l| l.accept()).await?;
        let (daemon, life) = (Arc::clone(daemon), Arc::clone(life));
        let rt = Arc::clone(&daemon.shared.rt);
        rt.spawn(Box::pin(async move {
            let _client = Client::new(&daemon);
            if let Err(e) = handle(&daemon, &life, stream).await {
                eprintln!("connection: {e}");
            }
        }));
    }
}

/// Counts one open connection for as long as it lives.
struct Client<'a>(&'a Daemon);

impl<'a> Client<'a> {
    fn new(daemon: &'a Daemon) -> Self {
        daemon.clients.fetch_add(1, Ordering::SeqCst);
        daemon.pulse.notify();
        Self(daemon)
    }
}

impl Drop for Client<'_> {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::SeqCst);
        self.0.pulse.notify();
    }
}

/// Answers every client that connects, until none waits (or, if none
/// came, for [`REFUSED_GRACE`]), with `why` in place of a welcome: for a
/// daemon that must not serve.
pub(crate) async fn refuse(reactor: &Reactor, listener: &Async<&Listener>, why: &str) {
    let Ok(frame) = Frame::new(&DaemonMsg::Error {
        message: why.to_owned(),
    }) else {
        return;
    };
    let first = within(reactor, REFUSED_GRACE, listener.read_with(|l| l.accept())).await;
    let mut next = first.and_then(Result::ok);
    while let Some(stream) = next.take() {
        if let Ok(io) = Async::new(reactor, stream) {
            let io = Arc::new(io);
            if write_all(&io, frame.as_bytes()).await.is_ok() {
                // What it sent is read and dropped until it hangs up.
                within(reactor, REFUSED_GRACE, Frames::new(io).drain()).await;
            }
        }
        next = listener.get_ref().accept().ok();
    }
}

/// `work`'s output, unless `limit` passes first.
async fn within<T>(reactor: &Reactor, limit: Duration, work: impl Future<Output = T>) -> Option<T> {
    let (mut work, mut late) = (pin!(work), reactor.sleep(limit));
    poll_fn(|cx| {
        if let Poll::Ready(done) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(done));
        }
        late.as_mut().poll(cx).map(|()| None)
    })
    .await
}

/// A connection's incoming frames, taken as they arrive.
struct Frames {
    io: Arc<Async<Stream>>,
    buf: Vec<u8>,
}

impl Frames {
    fn new(io: Arc<Async<Stream>>) -> Self {
        Self {
            io,
            buf: Vec::new(),
        }
    }

    /// The next message; `None` once the client hung up between frames.
    async fn next<T: Decode>(&mut self) -> io::Result<Option<T>> {
        loop {
            if let Some((len, body)) = protocol::frame_in(&self.buf)? {
                let msg = T::from_json(body)?;
                self.buf.drain(..len);
                return Ok(Some(msg));
            }
            let at = self.buf.len();
            self.buf.resize(at + (16 << 10), 0);
            let read = &mut self.buf[at..];
            let n = self.io.read_with(|mut s| s.read(read)).await;
            self.buf.truncate(at + n.as_ref().map_or(0, |n| *n));
            match n? {
                0 if at == 0 => return Ok(None),
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                _ => {}
            }
        }
    }

    /// Reads what the client sends and drops it, until it hangs up.
    async fn drain(mut self) {
        let mut scratch = vec![0; 16 << 10];
        while let Ok(1..) = self.io.read_with(|mut s| s.read(&mut scratch)).await {}
        self.buf.clear();
    }
}

/// Writes all of `bytes`, waiting whenever the socket is full.
async fn write_all(io: &Async<Stream>, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match io.write_with(|mut s| s.write(bytes)).await? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => bytes = &bytes[n..],
        }
    }
    Ok(())
}

/// Writes what `outbox` hands out until it closes, as many frames as are
/// ready at a time, up to [`WRITE_BATCH`] bytes: the client's cursor into
/// what it is sent, moved on as the socket takes it.
async fn write_out(io: Arc<Async<Stream>>, outbox: Outbox) {
    let mut batch = Vec::new();
    while let Some(frame) = outbox.next().await {
        batch.extend_from_slice(frame.as_bytes());
        while batch.len() < WRITE_BATCH
            && let Some(frame) = outbox.try_next()
        {
            batch.extend_from_slice(frame.as_bytes());
        }
        if write_all(&io, &batch).await.is_err() {
            outbox.abandon();
            return;
        }
        batch.clear();
    }
}

/// Serves connection `stream` until the client hangs up.
pub(crate) async fn handle(
    daemon: &Arc<Daemon>,
    life: &Lifecycle,
    stream: Stream,
) -> io::Result<()> {
    let io = Arc::new(Async::new(&daemon.shared.rt.reactor(), stream)?);
    let mut frames = Frames::new(Arc::clone(&io));
    let (verdict, theirs, caps, client) = match frames.next().await? {
        Some(ClientMsg::Hello {
            proto,
            build,
            min,
            caps,
            client,
        }) => {
            let theirs = Versions::new(min, proto);
            let caps = caps & Caps::SERVED;
            (life.verdict(theirs, &build), theirs, caps, client)
        }
        _ => (Verdict::Mismatch, Versions::from(0), Caps::NONE, None),
    };
    let outbox = Outbox::new();
    daemon
        .shared
        .rt
        .spawn(Box::pin(write_out(Arc::clone(&io), outbox.clone())));
    // Whatever the client sent behind a hello that is not welcomed is read
    // and dropped, never acted on.
    let result = match (verdict, Versions::OURS.common(theirs)) {
        (Verdict::Welcome, Some(proto)) => {
            let registered = life.register(outbox.clone());
            let mut conn = Conn {
                daemon,
                life,
                outbox: &outbox,
                number: registered.client(),
                // One that does not name itself is one client for as long
                // as its connection lasts.
                client: client.unwrap_or_default(),
                attached: None,
                decl: ClientDecl::default(),
            };
            conn.reply(&DaemonMsg::Welcome {
                proto,
                build: life.build.clone(),
                caps,
                client: conn.number,
                commands: driver::commands(&daemon.shared.registry.get()),
            });
            conn.serve(&mut frames).await
        }
        (Verdict::Drain, _) => {
            life.drain();
            daemon.pulse.notify();
            reply(&outbox, &DaemonMsg::Draining);
            // Held open until we exit: the client's cue to reconnect.
            frames.drain().await;
            Ok(())
        }
        _ => {
            reply(
                &outbox,
                &DaemonMsg::Mismatch {
                    supported: PROTO,
                    min: Versions::OURS.min_field(),
                },
            );
            within(&daemon.shared.rt.reactor(), REFUSED_GRACE, frames.drain()).await;
            Ok(())
        }
    };
    outbox.close();
    result
}

/// The workspace a client in `dir` works in: the git working tree `dir` is
/// in, found with the client's git variables (`GIT_DIR`, ceilings), not the
/// daemon's, so every client, local or bridged, gets the same rule.
fn workspace_of(dir: &Path, env: Option<&Environment>) -> PathBuf {
    let var = |name: &str| env?.vars.get(name).map(Into::into);
    sys::workspace_in(dir, &sys::GitEnv::from_vars(var))
}

pub(crate) fn reply(outbox: &Outbox, msg: &DaemonMsg) {
    // The writer ends with the connection; a lost reply needs no handling.
    if let Ok(frame) = Frame::new(msg) {
        let _ = outbox.send(frame);
    }
}

/// The answer to a mutation: done, or refused and why.
type Answer = Result<(), (Rejection, String)>;

/// A welcomed connection.
struct Conn<'a> {
    daemon: &'a Arc<Daemon>,
    life: &'a Lifecycle,
    outbox: &'a Outbox,
    /// The connection's number, as acks name it.
    number: u32,
    /// The client, as the log names who did what: a mutation it numbered
    /// that the log holds already is a resend, answered as the first was,
    /// across reconnects and daemon restarts.
    client: ClientId,
    attached: Option<Attachment>,
    decl: ClientDecl,
}

impl Conn<'_> {
    fn reply(&self, msg: &DaemonMsg) {
        reply(self.outbox, msg);
    }

    fn error(&self, message: String) {
        self.reply(&DaemonMsg::Error { message });
    }

    async fn serve(&mut self, frames: &mut Frames) -> io::Result<()> {
        while let Some(mut msg) = frames.next::<ClientMsg>().await? {
            if let Some(&mut client_seq) = msg.client_seq_mut() {
                let origin = client_seq.and_then(ClientSeq::new).map(|seq| Author {
                    client: self.client,
                    seq,
                });
                let answer = self.mutate(msg, origin).await;
                self.answered(client_seq, answer);
                continue;
            }
            match msg {
                ClientMsg::Attach {
                    target,
                    workspace,
                    since,
                    env,
                    decl,
                } => {
                    let found = || workspace_of(&workspace, env.as_ref());
                    match self.daemon.resolve(&target, found).await {
                        Ok(s) => {
                            self.decl = decl;
                            // Before the replay, so a client that saw itself
                            // attached knows its environment is recorded.
                            if let Some(env) = env {
                                s.session().set_env(env);
                            }
                            s.session().attach(self.outbox.clone(), since).await;
                            session_event("attach", s.session().id, || format!("since {since}"));
                            self.attached = Some(s);
                        }
                        Err(e) => self.error(e.to_string()),
                    }
                }
                ClientMsg::Declare(decl) => self.decl = decl,
                ClientMsg::Environment(env) => match &self.attached {
                    Some(s) => s.session().set_env(env),
                    None => self.error("not attached".into()),
                },
                ClientMsg::List => self.reply(&DaemonMsg::Sessions {
                    sessions: self.daemon.list(),
                }),
                ClientMsg::Settings { change } => {
                    let settings = self.settings(change).await;
                    self.reply(&settings);
                }
                ClientMsg::WatchSessions => self.daemon.watch_list(self.outbox),
                ClientMsg::Status => self.reply(&DaemonMsg::Status {
                    pid: std::process::id(),
                    build: self.life.build.clone(),
                    started: self.daemon.started,
                    clients: u32::try_from(self.daemon.clients.load(Ordering::SeqCst))
                        .unwrap_or(u32::MAX),
                    known: u32::try_from(lock(&self.daemon.slots).len()).unwrap_or(u32::MAX),
                }),
                ClientMsg::Shutdown if self.decl.role == Role::Viewer => {
                    self.error("this client is a viewer".into());
                }
                ClientMsg::Shutdown => {
                    self.daemon.stopping.store(true, Ordering::SeqCst);
                    self.daemon.pulse.notify();
                }
                ClientMsg::Detach => break,
                ClientMsg::Hello { .. }
                | ClientMsg::Input { .. }
                | ClientMsg::Steer { .. }
                | ClientMsg::Approve { .. }
                | ClientMsg::Interrupt { .. } => {}
            }
        }
        self.outbox.unfollow();
        Ok(())
    }

    /// The home settings, after `change` if this client may make it: the
    /// daemon is the file's one writer, writing it off the async workers.
    async fn settings(&self, change: Option<SettingsChange>) -> DaemonMsg {
        let shared = Arc::clone(&self.daemon.shared);
        let viewer = self.decl.role == Role::Viewer;
        let rt = Arc::clone(&shared.rt);
        let (file, refused) = rt::unblock(&*rt, move || {
            let home = &shared.models.settings.home;
            match change {
                None => (home.get(), None),
                Some(_) if viewer => (home.get(), Some("this client is a viewer".to_owned())),
                Some(change) => home
                    .change(&change)
                    .map_or_else(|e| (home.get(), Some(e)), |file| (file, None)),
            }
        })
        .await;
        let (settings, error) = match &*file {
            Ok(file) => (config::shown(file), refused),
            Err(e) => (HomeSettings::default(), refused.or_else(|| Some(e.clone()))),
        };
        DaemonMsg::Settings { settings, error }
    }

    /// Does mutation `msg`, from `origin`, if this client may.
    async fn mutate(&self, msg: ClientMsg, origin: Option<Author>) -> Answer {
        let session = match &self.attached {
            _ if self.decl.role == Role::Viewer => {
                return Err((Rejection::ReadOnly, "this client is a viewer".into()));
            }
            Some(s) => s.session(),
            None => return Err((Rejection::NotAttached, "not attached".into())),
        };
        let (request, refused) = match msg {
            ClientMsg::Interrupt { .. } => (Request::Interrupt, Rejection::Other),
            _ if self.life.draining().is_some() => {
                return Err((
                    Rejection::Draining,
                    "the daemon is restarting for an upgrade; send that again in a moment".into(),
                ));
            }
            ClientMsg::Input { text, .. } => {
                let mode = Mode::Queue;
                (Request::Input { text, mode }, Rejection::Refused)
            }
            ClientMsg::Steer { text, .. } => {
                let mode = Mode::Steer;
                (Request::Input { text, mode }, Rejection::Refused)
            }
            ClientMsg::Approve { call, approved, .. } => {
                let value = approved.into();
                (Request::Answer { call, value }, Rejection::NotWaiting)
            }
            _ => return Err((Rejection::Other, "not a mutation".into())),
        };
        let answer = session.request(request, origin).await;
        answer.map_err(|message| (refused, message))
    }

    /// Tells the client the answer to its mutation `client_seq`: an ack or
    /// rejection for a numbered one, else an error for a refusal.
    fn answered(&self, client_seq: Option<u64>, answer: Answer) {
        match (client_seq, answer) {
            (Some(client_seq), answer) => {
                let origin = Origin {
                    client: self.number,
                    client_seq,
                };
                self.reply(&match answer {
                    Ok(()) => DaemonMsg::Ack { origin },
                    Err((reason, message)) => DaemonMsg::Rejected {
                        origin,
                        reason,
                        message,
                    },
                });
            }
            (_, Err((_, message))) => self.error(message),
            (_, Ok(())) => {}
        }
    }
}
