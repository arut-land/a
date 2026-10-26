//! Connections, as tasks: a reader that takes a client's frames as they
//! arrive and a writer that follows the client's outbox, a cursor into the
//! session's frames. Both wait on the socket's readiness through the
//! reactor, so a connection costs no thread, and a client that stops
//! reading costs only its cursor.

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use protocol::{
    Author, Caps, ClientDecl, ClientId, ClientMsg, ClientSeq, DaemonMsg, Decode, Environment,
    Frame, HomeSettings, MachineId, PROTO, Rejection, Role, SettingsChange, Versions,
};
use rt::{Async, BoxFuture, Reactor};
use sys::{Listener, OwnedConnection as Stream};

use crate::driver::{self, Attachment, Request};
use crate::outbox::{BUDGET, Outbox};
use crate::query::Queries;
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
        let stream = match listener.read_with(|l| l.accept()).await {
            Ok(stream) => stream,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => continue,
            Err(error) => return Err(error),
        };
        if daemon.clients.load(Ordering::SeqCst) >= BUDGET.requests {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            continue;
        }
        let client = Client::new(daemon);
        let (daemon, life) = (Arc::clone(daemon), Arc::clone(life));
        let rt = Arc::clone(&daemon.shared.rt);
        rt.spawn(Box::pin(async move {
            let _client = client;
            if let Err(e) = handle(&daemon, &life, stream).await {
                ext_logging::emit(ext_logging::Fact::Fault {
                    session: None,
                    operation: ext_logging::Operation::Connection,
                    kind: ext_logging::Fault(e.kind()),
                });
            }
        }));
    }
}

/// Counts one open connection for as long as it lives.
struct Client(Arc<Daemon>);

impl Client {
    fn new(daemon: &Arc<Daemon>) -> Self {
        daemon.clients.fetch_add(1, Ordering::SeqCst);
        daemon.pulse.notify();
        Self(Arc::clone(daemon))
    }
}

impl Drop for Client {
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
            if io.write_all(frame.as_bytes()).await.is_ok() {
                // What it sent is read and dropped until it hangs up.
                within(reactor, REFUSED_GRACE, Frames::new(io).drain()).await;
            }
        }
        next = listener.get_ref().accept().ok();
    }
}

/// `work`'s output, unless `limit` passes first.
async fn within<T>(reactor: &Reactor, limit: Duration, work: impl Future<Output = T>) -> Option<T> {
    rt::or(async { Some(work.await) }, async {
        reactor.sleep(limit).await;
        None
    })
    .await
}

/// A connection's incoming frames, taken as they arrive.
struct Frames {
    io: Arc<Async<Stream>>,
    buf: Vec<u8>,
    bytes: usize,
    batch: usize,
}

impl Frames {
    fn new(io: Arc<Async<Stream>>) -> Self {
        Self {
            io,
            buf: Vec::new(),
            bytes: 0,
            batch: 0,
        }
    }

    /// The next message; `None` once the client hung up between frames.
    async fn next<T: Decode>(&mut self) -> io::Result<Option<T>> {
        loop {
            if self.batch >= WRITE_BATCH {
                self.batch = 0;
                rt::yield_now().await;
            }
            if let Some((len, body)) = protocol::frame_in(&self.buf)? {
                self.batch += len.max(WRITE_BATCH / 64);
                let msg = T::from_json(body.json()?)?;
                self.bytes = len;
                self.buf.drain(..len);
                return Ok(Some(msg));
            }
            let n = self
                .io
                .read_with(|mut s| {
                    // Scratch lives only while polling: an idle connection
                    // retains received bytes, not a full read buffer.
                    let mut read = [0; 16 << 10];
                    let n = s.read(&mut read)?;
                    self.buf.extend_from_slice(&read[..n]);
                    Ok(n)
                })
                .await?;
            match n {
                0 if self.buf.is_empty() => return Ok(None),
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

/// Writes what `outbox` hands out until it closes, as many frames as are
/// ready at a time, up to [`WRITE_BATCH`] bytes: the client's cursor into
/// what it is sent, moved on as the socket takes it.
async fn write_out(io: Arc<Async<Stream>>, outbox: Outbox, rt: rt::Rt) {
    let mut batch = Vec::new();
    while let Some(frame) = outbox.next(&*rt).await {
        batch.extend_from_slice(frame.as_bytes());
        while batch.len() < WRITE_BATCH
            && let Some(frame) = outbox.try_ready()
        {
            batch.extend_from_slice(frame.as_bytes());
        }
        if io.write_all(&batch).await.is_err() {
            outbox.abandon();
            let _ = io.get_ref().shutdown(std::net::Shutdown::Both);
            return;
        }
        batch.clear();
        rt::yield_now().await;
    }
    let _ = io.get_ref().shutdown(std::net::Shutdown::Both);
}

/// How a connection reached the daemon: its endpoint, the one way in, which
/// the OS opens to this user alone (a socket's peer credentials, a pipe's
/// per-user access list). A client bridged over ssh comes through it too,
/// ssh having authenticated this user on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Way {
    Endpoint,
}

/// The trust rule, and the one place a role is decided: this user, on a
/// way in that authenticated this user, owns the daemon. A way that does
/// not (another person's pinned key) is a new [`Way`], decided here.
const fn role_of(way: Way) -> Role {
    match way {
        Way::Endpoint => Role::Owner,
    }
}

/// Serves connection `stream` until the client hangs up.
pub(crate) async fn handle(
    daemon: &Arc<Daemon>,
    life: &Lifecycle,
    stream: Stream,
) -> io::Result<()> {
    handle_as(daemon, life, stream, role_of).await
}

/// [`handle`], with the connection's role as `role` decides it.
/// A client of the newer build: the daemon makes way for it as soon as
/// nothing holds the upgrade, and serves it meanwhile, if it can, saying
/// what does.
async fn upgrading(
    daemon: &Daemon,
    life: &Lifecycle,
    verdict: Verdict,
    theirs: Versions,
) -> (Verdict, Option<String>) {
    if verdict != Verdict::Drain || life.draining().is_some() {
        return (verdict, None);
    }
    life.want();
    daemon.pulse.notify();
    let held = daemon.upgrade_held(life.runtime()).await;
    let verdict = if held.is_some() && Versions::OURS.common(theirs).is_some() {
        Verdict::Welcome
    } else {
        verdict
    };
    (verdict, held)
}

pub(crate) async fn handle_as(
    daemon: &Arc<Daemon>,
    life: &Lifecycle,
    stream: Stream,
    role: fn(Way) -> Role,
) -> io::Result<()> {
    let io = Arc::new(Async::new(&daemon.shared.rt.reactor(), stream)?);
    let mut frames = Frames::new(Arc::clone(&io));
    let (verdict, theirs, caps, client, machine) = match frames.next().await? {
        Some(ClientMsg::Hello {
            proto,
            build,
            min,
            caps,
            client,
            machine,
        }) => {
            let theirs = Versions::new(min, proto);
            let caps = caps & Caps::SERVED;
            (life.verdict(theirs, &build), theirs, caps, client, machine)
        }
        _ => (Verdict::Mismatch, Versions::from(0), Caps::NONE, None, None),
    };
    let (verdict, held) = upgrading(daemon, life, verdict, theirs).await;
    let outbox = Outbox::new();
    let writer = write_out(
        Arc::clone(&io),
        outbox.clone(),
        Arc::clone(&daemon.shared.rt),
    );
    // A writer must not outlive the connection and its blocking admission.
    let (result, ()) = rt::zip(
        async {
            // Whatever the client sent behind a hello that is not welcomed is read
            // and dropped, never acted on.
            let result = match (verdict, Versions::OURS.common(theirs)) {
                (Verdict::Welcome, Some(proto)) => match daemon.machine.id().await {
                    Ok(ours) => {
                        let _registered = life.register(outbox.clone());
                        let mut conn = Conn {
                            daemon,
                            life,
                            outbox: &outbox,
                            // One that does not name itself is one client for as
                            // long as its connection lasts.
                            client: client.unwrap_or_default(),
                            // One that names no machine is on the daemon's.
                            machine: machine.unwrap_or(ours),
                            role: role(Way::Endpoint),
                            attached: None,
                            decl: ClientDecl::default(),
                            queries: Queries::default(),
                        };
                        let registry = daemon.shared.registry.get();
                        conn.reply(&DaemonMsg::Welcome {
                            proto,
                            build: life.build.clone(),
                            caps,
                            machine: ours,
                            commands: driver::commands(&registry),
                            queries: crate::query::advertised(&registry),
                            upgrade: held,
                        });
                        conn.serve(&mut frames).await
                    }
                    Err(message) => {
                        reply(&outbox, &DaemonMsg::Error { message });
                        within(&daemon.shared.rt.reactor(), REFUSED_GRACE, frames.drain()).await;
                        Ok(())
                    }
                },
                (Verdict::Drain, _) => {
                    let refused = held.is_some();
                    reply(&outbox, &DaemonMsg::Draining { held });
                    if refused {
                        within(&daemon.shared.rt.reactor(), REFUSED_GRACE, frames.drain()).await;
                    } else {
                        // Held open until we exit: the client's cue to reconnect.
                        frames.drain().await;
                    }
                    Ok(())
                }
                _ => {
                    ext_logging::emit(ext_logging::Fact::Mismatch {
                        min: theirs.min,
                        max: theirs.max,
                    });
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
        },
        writer,
    )
    .await;
    result
}

/// The workspace a client in `dir` works in: the git working tree `dir` is
/// in, found with the client's git variables (`GIT_DIR`, ceilings), not the
/// daemon's, so every client, local or bridged, gets the same rule.
pub(crate) fn workspace_of(dir: &Path, env: Option<&Environment>) -> PathBuf {
    let var = |name: &str| env?.vars.get(name).map(Into::into);
    sys::workspace_in(dir, &sys::GitEnv::from_vars(var))
}

pub(crate) fn reply(outbox: &Outbox, msg: &DaemonMsg) {
    let frame = Frame::new(msg).unwrap_or_else(|error| {
        let message = format!("framing reply: {error}");
        let failure = match msg {
            DaemonMsg::Reply { id, .. } => DaemonMsg::Reply {
                id: *id,
                outcome: protocol::Outcome::Failed { error: message },
            },
            _ => DaemonMsg::Error { message },
        };
        Frame::new(&failure).expect("framing failure fits in a frame")
    });
    let _ = outbox.send(frame);
}

/// The answer to a mutation: done, or refused and why.
type Answer = Result<(), (Rejection, String)>;

/// A welcomed connection.
struct Conn<'a> {
    daemon: &'a Arc<Daemon>,
    life: &'a Lifecycle,
    outbox: &'a Outbox,
    /// The client, as the log names who did what: a mutation it numbered
    /// that the log holds already is a resend, answered as the first was,
    /// across reconnects and daemon restarts.
    client: ClientId,
    /// The machine the client is on, as its hello claims: unauthenticated,
    /// since any process of this user may claim any machine, which the
    /// trust rule allows. For attribution, never for a decision.
    machine: MachineId,
    role: Role,
    attached: Option<Attachment>,
    decl: ClientDecl,
    queries: Queries,
}

impl Conn<'_> {
    fn reply(&self, msg: &DaemonMsg) {
        reply(self.outbox, msg);
    }

    fn error(&self, message: String) {
        self.reply(&DaemonMsg::Error { message });
    }

    /// Takes the client's frames as they come. Mutations are sent on at
    /// once and answered in order as they commit, while the next frames
    /// are read: an interrupt behind an input does not wait for its fsync.
    /// Queries, fetches and their cancels write no log, so none waits
    /// either.
    /// Anything else waits until what was asked before it is answered.
    #[expect(clippy::too_many_lines, reason = "one match over what a client sends")]
    async fn serve(&mut self, frames: &mut Frames) -> io::Result<()> {
        let mut pending: VecDeque<(Option<u64>, BoxFuture<'static, Answer>)> = VecDeque::new();
        let result = loop {
            let next = {
                let mut next = pin!(frames.next::<ClientMsg>());
                poll_fn(|cx| {
                    while let Some((client_seq, answer)) = pending.front_mut() {
                        let Poll::Ready(answer) = answer.as_mut().poll(cx) else {
                            break;
                        };
                        self.answered(*client_seq, answer);
                        pending.pop_front();
                    }
                    next.as_mut().poll(cx)
                })
                .await
            };
            let msg = match next {
                Ok(Some(msg)) => msg,
                ended => break ended.map(|_| ()),
            };
            let mut msg = match self.admit(msg) {
                Admission::Admitted(msg) => msg,
                Admission::Refused(mut refused) => {
                    // In turn with the mutations before it, as one taken would be.
                    if let Some(&mut client_seq) = refused.client_seq_mut() {
                        let answer = Err((Rejection::ReadOnly, VIEWER.into()));
                        pending.push_back((client_seq, Box::pin(std::future::ready(answer))));
                    } else {
                        while let Some((client_seq, answer)) = pending.pop_front() {
                            self.answered(client_seq, answer.await);
                        }
                        self.refuse(refused).await;
                    }
                    continue;
                }
            };
            if let Some(&mut client_seq) = msg.client_seq_mut() {
                let control = matches!(msg, ClientMsg::Interrupt { .. });
                if let Some(permit) = crate::outbox::QueuePermit::acquire(frames.bytes, control) {
                    let answer = self.mutate(msg);
                    pending.push_back((
                        client_seq,
                        Box::pin(async move {
                            let _permit = permit;
                            answer.await
                        }),
                    ));
                } else {
                    self.answered(
                        client_seq,
                        Err((Rejection::Refused, "daemon admission is full".into())),
                    );
                }
                continue;
            }
            let taken = (self.queries).take(
                self.daemon,
                self.outbox,
                self.machine,
                self.role,
                msg,
                frames.bytes,
            );
            let Some(msg) = taken else {
                continue;
            };
            while let Some((client_seq, answer)) = pending.pop_front() {
                self.answered(client_seq, answer.await);
            }
            match msg {
                ClientMsg::Attach {
                    target,
                    workspace,
                    since,
                    env,
                    decl,
                } => {
                    self.attach(target, workspace, since, env, decl).await;
                }
                ClientMsg::Declare(decl) => self.decl = decl,
                ClientMsg::Environment(env) => match &self.attached {
                    Some(s) => s
                        .session()
                        .set_env(env)
                        .unwrap_or_else(|error| self.error(error.to_string())),
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
                ClientMsg::Shutdown => {
                    lock(&self.daemon.stopping).get_or_insert(ext_logging::Stop::Requested);
                    self.daemon.pulse.notify();
                }
                ClientMsg::Detach => break Ok(()),
                ClientMsg::Hello { .. }
                | ClientMsg::Input { .. }
                | ClientMsg::Withdraw { .. }
                | ClientMsg::Approve { .. }
                | ClientMsg::Interrupt { .. }
                | ClientMsg::Query { .. }
                | ClientMsg::Cancel { .. }
                | ClientMsg::Fetch { .. } => {}
            }
        };
        while let Some((client_seq, answer)) = pending.pop_front() {
            self.answered(client_seq, answer.await);
        }
        self.outbox.unfollow();
        result
    }

    async fn attach(
        &mut self,
        target: protocol::Target,
        workspace: Option<protocol::HostPath>,
        since: u64,
        env: Option<Environment>,
        decl: ClientDecl,
    ) {
        let env = if matches!(target, protocol::Target::Fork { .. }) {
            None
        } else {
            env
        };
        // An attach by id or prefix never needs filesystem discovery.
        let workspace = if matches!(
            target,
            protocol::Target::New
                | protocol::Target::Latest {
                    any_workspace: false
                }
        ) {
            let env = env.clone();
            Some(
                rt::unblock(&*self.daemon.shared.rt, move || {
                    let dir = workspace
                        .as_ref()
                        .map_or(Path::new(""), protocol::HostPath::local);
                    workspace_of(dir, env.as_ref())
                })
                .await,
            )
        } else {
            None
        };
        let found = || workspace.expect("target requires a workspace");
        let resolved = match &target {
            protocol::Target::Fork { session, seq } => {
                match self.daemon.fork(session, *seq).await {
                    Ok(session) => Ok(session),
                    Err(reason) => {
                        self.reply(&DaemonMsg::ForkRejected { reason });
                        return;
                    }
                }
            }
            _ => self.daemon.resolve(&target, found).await,
        };
        match resolved {
            Ok(s) => {
                self.decl = decl;
                // Before the replay, so a client that saw itself
                // attached knows its environment is recorded.
                if let Some(env) = env
                    && let Err(error) = s.session().set_env(env)
                {
                    self.error(error.to_string());
                    return;
                }
                s.session().attach(self.outbox.clone(), since).await;
                session_event("attach", s.session().id, || {
                    format!("since {since} from {}", self.machine)
                });
                self.attached = Some(s);
            }
            Err(e) => self.error(e.to_string()),
        }
    }

    /// What this connection may ask, checked once here for every message
    /// before anything acts on it, so that no request skips it: a viewer's
    /// that would write (a log, the home's settings, the daemon's life) is
    /// handed back, refused, and its attach keeps the session's
    /// environment.
    fn admit(&self, msg: ClientMsg) -> Admission {
        match self.role {
            Role::Owner => Admission::Admitted(msg),
            Role::Viewer => match msg {
                msg if writes(&msg) => Admission::Refused(msg),
                ClientMsg::Attach {
                    target,
                    workspace,
                    since,
                    decl,
                    env: _,
                } => Admission::Admitted(ClientMsg::Attach {
                    target,
                    workspace,
                    since,
                    env: None,
                    decl,
                }),
                msg => Admission::Admitted(msg),
            },
        }
    }

    /// Answers a request [`Conn::admit`] refused, as its kind is answered.
    async fn refuse(&self, msg: ClientMsg) {
        match msg {
            ClientMsg::Attach {
                target: protocol::Target::Fork { .. },
                ..
            } => self.reply(&DaemonMsg::ForkRejected {
                reason: protocol::ForkError::Viewer,
            }),
            ClientMsg::Settings { .. } => {
                let DaemonMsg::Settings { settings, .. } = self.settings(None).await else {
                    unreachable!("settings answers with settings");
                };
                self.reply(&DaemonMsg::Settings {
                    settings,
                    error: Some(VIEWER.into()),
                });
            }
            _ => self.error(VIEWER.into()),
        }
    }

    /// The home settings, after `change`: the daemon is the file's one
    /// writer, writing it off the async workers.
    async fn settings(&self, change: Option<SettingsChange>) -> DaemonMsg {
        let shared = Arc::clone(&self.daemon.shared);
        let rt = Arc::clone(&shared.rt);
        let (file, refused) = rt::unblock(&*rt, move || {
            let home = &shared.models.settings.home;
            home.refresh();
            match change {
                None => (home.get(), None),
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

    /// Sends mutation `msg`, from `origin`, on: the future is its answer.
    fn mutate(&self, msg: ClientMsg) -> BoxFuture<'static, Answer> {
        let origin = msg.client_seq().and_then(ClientSeq::new).map(|seq| Author {
            client: self.client,
            seq,
        });
        let refused =
            |answer: Answer| -> BoxFuture<'static, Answer> { Box::pin(std::future::ready(answer)) };
        let session = match &self.attached {
            Some(s) => s.session(),
            None => return refused(Err((Rejection::NotAttached, "not attached".into()))),
        };
        let (request, rejection) = match msg {
            ClientMsg::Interrupt { .. } => (Request::Interrupt, Rejection::Other),
            _ if self.life.draining().is_some() => {
                return refused(Err((Rejection::Draining, crate::upgrade::DRAINING.into())));
            }
            ClientMsg::Input {
                segments,
                mode,
                force,
                ..
            } => (
                Request::Input {
                    segments,
                    mode,
                    force,
                },
                Rejection::Refused,
            ),
            ClientMsg::Withdraw { input, .. } => {
                (Request::Withdraw { input }, Rejection::NotWaiting)
            }
            ClientMsg::Approve { call, approved, .. } => {
                let value = protocol::Json::encode(&approved).unwrap_or_default();
                (Request::Answer { call, value }, Rejection::NotWaiting)
            }
            _ => return refused(Err((Rejection::Other, "not a mutation".into()))),
        };
        let answer = session.ask(request, origin);
        // One a handover refuses as it commits is to be sent again.
        let draining = self.life.drain_cell();
        Box::pin(async move {
            answer.await.map_err(|message| {
                let rejection = if draining.get().is_some() {
                    Rejection::Draining
                } else {
                    rejection
                };
                (rejection, message)
            })
        })
    }

    /// Tells the client the answer to its mutation `client_seq`: an ack or
    /// rejection for a numbered one, else an error for a refusal.
    fn answered(&self, client_seq: Option<u64>, answer: Answer) {
        match (client_seq, answer) {
            (Some(client_seq), answer) => {
                self.reply(&match answer {
                    Ok(()) => DaemonMsg::Ack { client_seq },
                    Err((reason, message)) => DaemonMsg::Rejected {
                        client_seq,
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

/// A request as [`Conn::admit`] takes it: to act on, or to refuse.
enum Admission {
    Admitted(ClientMsg),
    Refused(ClientMsg),
}

/// Why a viewer's request is refused.
const VIEWER: &str = "this client is a viewer";

/// Whether `msg` would write: a session's log (a mutation, an environment,
/// a new or forked session), the home's settings, or the daemon's life.
/// An attach's environment is not counted: a viewer's is dropped instead.
const fn writes(msg: &ClientMsg) -> bool {
    use protocol::Target;
    match msg {
        ClientMsg::Input { .. }
        | ClientMsg::Withdraw { .. }
        | ClientMsg::Approve { .. }
        | ClientMsg::Interrupt { .. }
        | ClientMsg::Environment(_)
        | ClientMsg::Shutdown
        | ClientMsg::Settings { change: Some(_) }
        | ClientMsg::Attach {
            target: Target::New | Target::Fork { .. },
            ..
        } => true,
        ClientMsg::Hello { .. }
        | ClientMsg::Attach {
            target: Target::Latest { .. } | Target::Id(_) | Target::Prefix(_),
            ..
        }
        | ClientMsg::Declare(_)
        | ClientMsg::List
        | ClientMsg::Detach
        | ClientMsg::WatchSessions
        | ClientMsg::Status
        | ClientMsg::Settings { change: None }
        | ClientMsg::Query { .. }
        | ClientMsg::Cancel { .. }
        | ClientMsg::Fetch { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt::Runtime as _;
    use std::io::Write;

    #[test]
    fn incoming_frames_keep_only_received_bytes_and_yield_between_batches() {
        let root =
            std::env::temp_dir().join(format!("a-server-overload-{}", protocol::SessionId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let home = sys::Home::from(root.clone());
        std::fs::create_dir_all(home.run_dir()).unwrap();
        let owner = std::fs::File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(home.daemon_pid_file())
            .unwrap();
        sys::fs::lock(&owner).unwrap();
        Write::write_all(
            &mut &owner,
            sys::crash::Record::current("test").to_string().as_bytes(),
        )
        .unwrap();
        let listener = home.bind().unwrap();
        let peer = home.endpoint().connect().unwrap();
        let stream = listener.accept().unwrap();
        let runtime = rt::Threads::new(1);
        let mut frames = Frames::new(Arc::new(Async::new(&runtime.reactor(), stream).unwrap()));
        ClientMsg::Detach.write_frame(&mut &peer).unwrap();
        assert!(matches!(
            rt::block_on(frames.next::<ClientMsg>()).unwrap(),
            Some(ClientMsg::Detach)
        ));
        assert!(
            frames.buf.capacity() < 16 << 10,
            "read scratch was retained"
        );
        let writer = std::thread::spawn(move || {
            ClientMsg::Hello {
                proto: PROTO,
                build: "x".repeat(64 << 10),
                min: None,
                caps: Caps::NONE,
                client: None,
                machine: None,
            }
            .write_frame(&mut &peer)
            .unwrap();
            peer
        });
        assert!(matches!(
            rt::block_on(frames.next::<ClientMsg>()).unwrap(),
            Some(ClientMsg::Hello { build, .. }) if build.len() == 64 << 10
        ));
        let peer = writer.join().unwrap();
        frames.batch = 0;
        for _ in 0..65 {
            ClientMsg::Detach.write_frame(&mut frames.buf).unwrap();
        }
        for _ in 0..64 {
            assert!(matches!(
                rt::block_on(frames.next::<ClientMsg>()).unwrap(),
                Some(ClientMsg::Detach)
            ));
        }
        {
            let mut next = pin!(frames.next::<ClientMsg>());
            rt::block_on(poll_fn(|cx| {
                assert!(next.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            }));
            assert!(matches!(
                rt::block_on(next).unwrap(),
                Some(ClientMsg::Detach)
            ));
        }
        drop(frames);
        drop(peer);
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(root).unwrap();
    }
}
