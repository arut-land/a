//! The client SDK every app is a renderer over. Below, the transport: a
//! [`link::Link`] reaches a daemon (this machine's over its socket, spawned
//! if needed; another machine's through `ssh <host> a bridge`, whose stdio
//! carries the same frames) and one hello runs over any of them. Above it, without I/O, a
//! session's link, view and intents ([`SessionClient`]) and the one slash
//! command table ([`COMMANDS`]); a [`Driver`] owns transport on the supplied runtime.

mod approval;
mod command;
mod daemons;
mod drive;
mod fetch;
mod host;
mod inbox;
pub mod link;
mod machines;
mod query;
mod remote;
mod session;
mod stream;
mod submission;
pub use stream::{ReplyEvent, ReplyStream, Sanitizer, sanitize};
pub use submission::{Submission, SubmissionId, SubmissionState};
mod watch;

pub use approval::{ApprovalPart, approval_parts, approval_text};
pub use command::{COMMANDS, Cmd, Command, help, parse, parse_input};
pub use daemons::{Daemons, Listed, Member};
pub use drive::{ClientHandle, Driver, Renderer, pump};
pub use fetch::{Fetched, Unfetched};
pub use host::Host;
pub use inbox::{Inbox, PUMP_BATCH, Post, QueueBudget, inbox};
pub use link::{LinkError, LocalLink, Start};
pub use machines::{Known, Machine, MachineWatch, Route, Sources, SshConfig};
pub use query::{Answer, Answers, Canceller};
pub use remote::SshLink;
pub use session::{
    Attach, ConnId, Effect, End, Env, Event, EventKind, Io, Link, Notice, Offer, SessionClient,
    Show, TurnView,
};
pub use watch::{Problem, SessionWatch};

use std::fmt;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use protocol::view::SessionList;
use protocol::{
    Caps, ClientDecl, ClientId, ClientMsg, CommandInfo, DaemonMsg, Environment, HomeSettings,
    MachineId, QueryInfo, SessionId, SessionSummary, SettingsChange, write_frame,
};
use sys::crash::{Crashes, Record};
use sys::{Home, OwnedConnection, Signal, Staged};

use link::Greeting;

/// Why a daemon could not be reached, or stopped serving this client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The daemon speaks `theirs`, not this build's protocol. Staging never
    /// downgrades, so when a `newer` build is staged, restarting the daemon
    /// would only start it again: that build is the one to run. On another
    /// `host`, one side or the other is to be updated.
    Mismatch {
        theirs: u32,
        newer: Option<PathBuf>,
        host: Option<Host>,
    },
    /// The daemon took the connection but did not answer the hello in
    /// time; see [`diagnose`] and [`restart`].
    NotResponding,
    /// The daemon refuses to serve, as it says (after a crash loop).
    Refused(String),
    Fork(protocol::ForkError),
    /// No additional request was admitted; a pending environment request keeps its latest value.
    Busy,
    /// The daemon closed the connection.
    Closed,
    /// The link to the daemon could not be opened, as it says.
    Link(LinkError),
    /// The daemon reached names a machine other than the one pinned for
    /// it: the host now leads elsewhere, or the machine was made anew.
    OtherMachine {
        pinned: MachineId,
        found: MachineId,
    },
    /// The socket, `ssh` or the spawn failed.
    Io(io::ErrorKind, String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mine = protocol::PROTO;
        match self {
            Self::Mismatch {
                theirs,
                newer: Some(newer),
                ..
            } => write!(
                f,
                "the daemon runs a newer a (protocol {theirs}) than this one (protocol {mine}); run {} instead, or update this a",
                newer.display()
            ),
            Self::Mismatch {
                theirs,
                host: Some(host),
                ..
            } => write!(
                f,
                "the daemon on {host} speaks protocol {theirs}, this a protocol {mine}; update a on one side"
            ),
            Self::Mismatch { theirs, .. } => {
                write!(
                    f,
                    "the daemon speaks protocol {theirs}, this a protocol {mine}; restart the daemon"
                )
            }
            Self::Fork(reason) => reason.fmt(f),
            Self::Link(why) => why.fmt(f),
            Self::NotResponding => f.write_str("the daemon is not responding"),
            Self::Refused(why) | Self::Io(_, why) => f.write_str(why),
            Self::Busy => f.write_str("the client transport is busy; try again"),
            Self::OtherMachine { pinned, found } => write!(
                f,
                "the daemon reached is machine {found}, not {pinned} as machines.toml pins"
            ),
            Self::Closed => f.write_str("the daemon closed the connection"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Self::NotResponding,
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe => Self::Closed,
            kind => Self::Io(kind, e.to_string()),
        }
    }
}

/// For the commands that report through `io::Result`.
impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = match &e {
            Error::Mismatch { .. } => io::ErrorKind::Unsupported,
            Error::NotResponding => io::ErrorKind::TimedOut,
            Error::Refused(_) | Error::Fork(_) => io::ErrorKind::Other,
            Error::Busy => io::ErrorKind::WouldBlock,
            Error::Link(LinkError::NeedsInteractiveAuth(_)) => io::ErrorKind::PermissionDenied,
            Error::Link(LinkError::Unreachable(_)) => io::ErrorKind::HostUnreachable,
            Error::Closed => io::ErrorKind::UnexpectedEof,
            Error::OtherMachine { .. } => io::ErrorKind::InvalidData,
            Error::Io(kind, _) => *kind,
        };
        Self::new(kind, e.to_string())
    }
}

/// A daemon connection, for a client that waits on each answer in turn;
/// [`Driver`] reads and writes one at once.
pub struct Connection {
    stream: Box<dyn link::Stream>,
    buffered: Vec<u8>,
    /// The far host's environment, as its bridge captured it; `None` for
    /// this machine's daemon.
    pub remote: Option<Environment>,
    /// The daemon's machine, as its welcome named it.
    pub machine: MachineId,
    /// What the hello settled.
    pub welcome: Welcome,
    /// The slash commands the daemon advertised.
    pub commands: Vec<CommandInfo>,
    /// The queries it answers.
    pub queries: Vec<QueryInfo>,
    /// What holds the upgrade to a newer build the daemon waits for, if
    /// it waits: it serves this client meanwhile.
    pub upgrade: Option<String>,
}

/// What the daemon's welcome settled for a connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Welcome {
    pub proto: u32,
    /// The capabilities taken up.
    pub caps: Caps,
}

impl Connection {
    /// The environment a session attached over this connection runs its
    /// commands with: this process's, or a remote daemon's bridge's, never
    /// this machine's paths on another.
    pub fn origin(&self) -> io::Result<Environment> {
        origin(self.remote.as_ref())
    }

    /// The message making `attach`. The daemon finds the workspace from
    /// the directory given, else from the origin's; an attach that keeps
    /// the session's environment captures nothing.
    pub fn attach(&self, attach: &Attach) -> io::Result<ClientMsg> {
        attaching(attach, || self.origin())
    }

    /// Sends `msg` as it is: a mutation is numbered by the session client
    /// that made it, which keeps it for the daemon's answer.
    pub fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        rt::block_on(self.stream.send(msg))
    }

    /// Sends each of `msgs`.
    pub fn send_all(&mut self, msgs: &[ClientMsg]) -> io::Result<()> {
        msgs.iter().try_for_each(|msg| self.send(msg))
    }

    pub fn recv(&mut self) -> io::Result<Option<DaemonMsg>> {
        rt::block_on(self.stream.recv(&mut self.buffered))
    }

    /// The first message `pick` accepts; the rest are skipped.
    pub fn until<T>(&mut self, mut pick: impl FnMut(DaemonMsg) -> Option<T>) -> io::Result<T> {
        rt::block_on(until(&*self.stream, &mut self.buffered, &mut pick))
    }

    /// The home settings, after making `change` to them. The daemon is
    /// their one writer; a change it refuses (a bad variable name, a file
    /// it cannot parse) is `InvalidInput`, saying why.
    pub fn settings(mut self, change: Option<SettingsChange>) -> io::Result<HomeSettings> {
        self.send(&ClientMsg::Settings { change })?;
        let (settings, error) = self.until(|msg| match msg {
            DaemonMsg::Settings { settings, error } => Some((settings, error)),
            _ => None,
        })?;
        match error {
            Some(error) => Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
            None => Ok(settings),
        }
    }

    /// Asks for the session list now and on every change, as
    /// [`DaemonMsg::Sessions`].
    pub fn watch_sessions(&mut self) -> io::Result<()> {
        self.send(&ClientMsg::WatchSessions)
    }

    /// The daemon's sessions, newest first.
    pub fn sessions(self) -> io::Result<Vec<SessionSummary>> {
        rt::block_on(self.sessions_async())
    }

    /// Lists sessions without blocking the calling task.
    pub async fn sessions_async(mut self) -> io::Result<Vec<SessionSummary>> {
        self.stream.send(&ClientMsg::List).await?;
        let mut list = SessionList::default();
        until(&*self.stream, &mut self.buffered, &mut |msg| {
            list.fold(&msg).then(|| std::mem::take(&mut list.sessions))
        })
        .await
    }
}

/// The first message `pick` accepts; the rest are skipped.
async fn until<T>(
    stream: &dyn link::Stream,
    buffered: &mut Vec<u8>,
    pick: &mut impl FnMut(DaemonMsg) -> Option<T>,
) -> io::Result<T> {
    while let Some(msg) = stream.recv(buffered).await? {
        if let Some(found) = pick(msg) {
            return Ok(found);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        Error::Closed.to_string(),
    ))
}

/// Where a session's commands run: the far host's environment, as its
/// bridge captured it, else this process's.
fn origin(remote: Option<&Environment>) -> io::Result<Environment> {
    remote.map_or_else(capture, |env| Ok(env.clone()))
}

fn attaching(
    attach: &Attach,
    origin: impl FnOnce() -> io::Result<Environment>,
) -> io::Result<ClientMsg> {
    let env = match &attach.env {
        Env::Keep => None,
        Env::Origin => Some(origin()?),
        Env::Given(env) => Some(env.clone()),
    };
    let workspace = (attach.workspace.clone()).or_else(|| env.as_ref().map(|env| env.cwd.clone()));
    Ok(ClientMsg::Attach {
        target: attach.target.clone(),
        workspace,
        since: attach.since,
        env,
        decl: ClientDecl::default(),
    })
}

/// Why no one session matches a prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoMatch {
    None,
    Several,
}

/// The one of `sessions` whose id (as `id` finds it), in full or short
/// form, starts with `prefix`.
pub fn matching<'a, T>(
    sessions: &'a [T],
    id: impl Fn(&T) -> Option<SessionId>,
    prefix: &str,
) -> Result<&'a T, NoMatch> {
    let mut found = sessions.iter().filter(|s| {
        id(s).is_some_and(|id| {
            id.short().to_string().starts_with(prefix) || id.to_string().starts_with(prefix)
        })
    });
    match (found.next(), found.next()) {
        (Some(one), None) => Ok(one),
        (None, _) => Err(NoMatch::None),
        (Some(_), Some(_)) => Err(NoMatch::Several),
    }
}

/// This process's environment, for the commands a session runs for it.
pub fn capture() -> io::Result<Environment> {
    Ok(Environment::new(
        std::env::current_dir()?.try_into()?,
        std::env::vars_os(),
        io::stdin().is_terminal(),
    ))
}

/// A daemon as a client reaches it: the link to it, this build (which a
/// connection may start, or install on a host that lacks `a`), and the
/// machine it must be, where that is pinned.
#[derive(Clone, Debug)]
pub struct Daemon {
    pub link: Arc<dyn link::Link>,
    pub staged: Staged,
    pub pinned: Option<MachineId>,
}

impl Daemon {
    pub fn new(link: impl link::Link, staged: Staged) -> Self {
        Self {
            link: Arc::new(link),
            staged,
            pinned: None,
        }
    }

    /// This machine's daemon.
    pub fn local(home: Home, staged: Staged) -> Self {
        Self::new(LocalLink::new(home), staged)
    }

    /// Connects and completes the hello. A daemon draining for a newer
    /// build is waited out (`on_drain` says so first), then the next
    /// attempt starts the new build.
    pub fn connect(&self, on_drain: &(dyn Fn() + Sync)) -> Result<Connection, Error> {
        self.connect_as(None, on_drain, &|_| Ok(Vec::new()))
    }

    /// [`Daemon::connect`] as `client`, which names the client across its
    /// connections so a mutation it sends again is done once, sending the
    /// messages `first` makes for the connection (given the far host's
    /// environment, if any) right behind the hello rather than after the
    /// welcome, which saves a round trip.
    pub fn connect_as(
        &self,
        client: Option<ClientId>,
        on_drain: &(dyn Fn() + Sync),
        first: &(dyn Fn(Option<&Environment>) -> io::Result<Vec<ClientMsg>> + Sync),
    ) -> Result<Connection, Error> {
        self.open(client, Start::Reuse, on_drain, first)
    }

    fn open(
        &self,
        client: Option<ClientId>,
        start: Start,
        on_drain: &(dyn Fn() + Sync),
        first: &(dyn Fn(Option<&Environment>) -> io::Result<Vec<ClientMsg>> + Sync),
    ) -> Result<Connection, Error> {
        rt::block_on(self.open_async(&link::BLOCKING, client, start, on_drain, first))
    }

    /// Connects on the supplied runtime, yielding while the daemon answers.
    pub async fn connect_async(
        &self,
        rt: &rt::Rt,
        on_drain: &(dyn Fn() + Sync),
    ) -> Result<Connection, Error> {
        self.connect_as_async(rt, None, on_drain, &|_| Ok(Vec::new()))
            .await
    }

    /// [`Daemon::connect_as`] without blocking the calling task.
    pub async fn connect_as_async(
        &self,
        rt: &rt::Rt,
        client: Option<ClientId>,
        on_drain: &(dyn Fn() + Sync),
        first: &(dyn Fn(Option<&Environment>) -> io::Result<Vec<ClientMsg>> + Sync),
    ) -> Result<Connection, Error> {
        self.open_async(rt, client, Start::Reuse, on_drain, first)
            .await
    }

    async fn open_async(
        &self,
        rt: &rt::Rt,
        client: Option<ClientId>,
        start: Start,
        on_drain: &(dyn Fn() + Sync),
        first: &(dyn Fn(Option<&Environment>) -> io::Result<Vec<ClientMsg>> + Sync),
    ) -> Result<Connection, Error> {
        let result = link::connect(rt, self, client, start, first, on_drain).await;
        if matches!(result, Err(Error::NotResponding)) {
            self.link.unanswered();
        }
        result
    }

    /// The daemon's sessions, newest first.
    pub fn sessions(&self) -> Result<Vec<SessionSummary>, Error> {
        Ok(self.connect(&|| {})?.sessions()?)
    }

    /// Starts a session in the origin's working directory with its
    /// environment, as `a new` does, and leaves it running.
    pub fn new_session(&self) -> Result<SessionId, Error> {
        let new = Attach::new(protocol::Target::New, Env::Origin);
        let mut conn = self.connect_as(None, &|| {}, &|remote| {
            Ok(vec![attaching(&new, || origin(remote))?])
        })?;
        let session = conn.until(|msg| match msg {
            DaemonMsg::Attached { session, .. } => Some(session),
            _ => None,
        })?;
        conn.send(&ClientMsg::Detach)?;
        Ok(session)
    }

    /// Copies a conversation through `seq` (or its latest entry) into an
    /// independent session. Files stay as they are; the recorded environment
    /// is retained, without secrets that were redacted from the log.
    pub fn fork_session(
        &self,
        session: String,
        seq: Option<protocol::Seq>,
    ) -> Result<SessionId, Error> {
        let attach = Attach::new(protocol::Target::Fork { session, seq }, Env::Keep);
        let mut conn = self.connect_as(None, &|| {}, &|remote| {
            Ok(vec![attaching(&attach, || origin(remote))?])
        })?;
        let session = conn.until(|msg| match msg {
            DaemonMsg::Attached { session, .. } => Some(Ok(session)),
            DaemonMsg::ForkRejected { reason } => Some(Err(Error::Fork(reason))),
            DaemonMsg::Error { message } => Some(Err(Error::Refused(message))),
            _ => None,
        })??;
        conn.send(&ClientMsg::Detach)?;
        Ok(session)
    }

    /// Stops any running daemon and connects to a fresh one, which serves
    /// even after the crash-loop breaker stopped the last ones.
    pub fn restart(&self) -> Result<Connection, Error> {
        self.open(None, Start::Replace, &|| {}, &|_| Ok(Vec::new()))
    }
}

/// How long a daemon may take to answer the hello before it counts as wedged.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// [`HELLO_TIMEOUT`] for a daemon this client just started: the first run
/// of a newly staged build is scanned by macOS first, which takes half a
/// second on an idle machine and seconds on a busy one.
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a draining daemon before calling it wedged. A
/// daemon drains only once it is making way, which takes no longer than
/// its shutdown; an upgrade that must wait serves the client meanwhile.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(75);

/// What a client can tell about the daemon without talking to it.
#[derive(Clone, Debug, Default)]
pub struct Diagnosis {
    /// The pid recorded by a daemon that still holds its lock.
    pub pid: Option<u32>,
    /// What that daemon recorded about itself.
    pub record: Option<Record>,
    pub log: PathBuf,
    /// The last crash.
    pub crashes: Crashes,
}

impl Diagnosis {
    /// The daemon, by pid when one holds its lock.
    pub fn who(&self) -> String {
        self.pid.map_or_else(
            || "the daemon".to_owned(),
            |pid| format!("the daemon (pid {pid})"),
        )
    }

    /// The last lines of the daemon's log.
    pub fn log_tail(&self) -> Vec<String> {
        const LINES: usize = 10;
        let text = fs::read_to_string(&self.log).unwrap_or_default();
        let mut tail: Vec<String> = text.lines().rev().take(LINES).map(str::to_owned).collect();
        tail.reverse();
        tail
    }

    /// What to tell a person about the last crash, if any.
    pub fn crash_note(&self) -> Option<String> {
        let last = self.crashes.last.as_ref()?;
        let summary = fs::read_to_string(last)
            .ok()
            .and_then(|text| text.lines().next().map(str::to_owned))
            .unwrap_or_default();
        Some(format!("last crash: {summary} ({})", last.display()))
    }
}

pub fn diagnose(home: &Home) -> Diagnosis {
    let record = live_daemon(home);
    Diagnosis {
        pid: record.as_ref().map(|r| r.pid),
        record,
        log: home.daemon_log(),
        crashes: Crashes::of(home),
    }
}

/// The running daemon's record. Only trusted while the daemon holds its
/// lock: if we can take the lock, the daemon is gone and the pid may be
/// reused.
pub(crate) fn live_daemon(home: &Home) -> Option<Record> {
    let file = File::open(home.daemon_pid_file()).ok()?;
    if sys::fs::try_lock(&file).is_ok() {
        return None;
    }
    fs::read_to_string(home.daemon_pid_file())
        .ok()?
        .parse()
        .ok()
}

/// Stops the daemon: asked over its socket, as an idle one stops; else, as
/// for one too wedged to answer, SIGTERM, then
/// SIGKILL if it still holds its lock after `HELLO_TIMEOUT`. Each signal
/// goes only to the process the daemon recorded, checked by its start time
/// as well as its pid. Returns whether a daemon was running.
pub fn stop(home: &Home) -> io::Result<bool> {
    rt::block_on(stop_async(&link::BLOCKING, home))
}

pub(crate) async fn stop_async(rt: &rt::Rt, home: &Home) -> io::Result<bool> {
    let Some(daemon) = live_daemon(home) else {
        return Ok(false);
    };
    if ask_to_stop(rt, home, &daemon.build).await.is_ok()
        && wait_exited_async(rt, home, Some(HELLO_TIMEOUT)).await
    {
        return Ok(true);
    }
    let process = sys::RecordedProcess::open(&daemon)?;
    ext_logging::witness(home, &daemon, ext_logging::Observation::Terminate);
    process.signal(Signal::Terminate)?;
    if !wait_exited_async(rt, home, Some(HELLO_TIMEOUT)).await {
        ext_logging::witness(home, &daemon, ext_logging::Observation::Kill);
        process.signal(Signal::Kill)?;
        wait_exited_async(rt, home, None).await;
    }
    home.endpoint().remove();
    // Stopped on purpose: the record it left is not a crash.
    if let Ok(file) = File::options().write(true).open(home.daemon_pid_file())
        && sys::fs::try_lock(&file).is_ok()
    {
        file.set_len(0)?;
    }
    Ok(true)
}

/// Whether the daemon let go of its lock, as it does when it exits, within
/// `within` (or ever). Contention retries nonblocking native lock attempts
/// on the runtime's timer; cancelling a wait leaves no blocking job behind.
pub fn wait_exited(home: &Home, within: Option<Duration>) -> bool {
    rt::block_on(wait_exited_async(&link::BLOCKING, home, within))
}

async fn wait_exited_async(rt: &rt::Rt, home: &Home, within: Option<Duration>) -> bool {
    let Ok(file) = File::open(home.daemon_pid_file()) else {
        return true;
    };
    locked(rt, file, within).await.is_some()
}

/// Lock holders and shutdown need Storage workers themselves, so a waiter
/// must never occupy one while it waits for them.
async fn locked(rt: &rt::Rt, file: File, within: Option<Duration>) -> Option<File> {
    let lock = async {
        loop {
            match sys::fs::try_lock(&file) {
                Ok(()) => return Some(file),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    rt.sleep(Duration::from_millis(25)).await;
                }
                Err(_) => return None,
            }
        }
    };
    match within {
        Some(within) => {
            rt::or(lock, async {
                rt.sleep(within).await;
                None
            })
            .await
        }
        None => lock.await,
    }
}

/// Local endpoint verification and named-pipe opening may block in the OS.
async fn local_connect(rt: &rt::Rt, home: &Home) -> io::Result<OwnedConnection> {
    let endpoint = home.endpoint();
    rt::unblock(&**rt, move || endpoint.connect()).await
}

/// Connects to the running daemon, as its own build `build` so as not to
/// set off a drain, without starting one.
async fn reach(rt: &rt::Rt, home: &Home, build: &str) -> Result<Connection, Error> {
    let link::Opened {
        stream,
        machine,
        mut buffered,
        remote,
        within,
    } = link::socket(rt, local_connect(rt, home).await?)?;
    let greeting = answered(
        rt,
        within,
        link::hello(&*stream, &mut buffered, build, None, machine, &[]),
    )
    .await?;
    match greeting {
        (Greeting::Welcome(theirs, upgrade), welcome, commands, queries) => Ok(Connection {
            stream,
            buffered,
            remote,
            machine: theirs,
            welcome,
            commands,
            queries,
            upgrade,
        }),
        _ => Err(Error::Refused("the daemon cannot be asked".to_owned())),
    }
}

/// `future`, unless `within` passes first: a daemon that took the
/// connection but does not answer.
async fn answered<T>(
    rt: &rt::Rt,
    within: Duration,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    rt::or(future, async {
        rt.sleep(within).await;
        Err(Error::NotResponding)
    })
    .await
}

/// Asks the daemon to stop, and waits for it to close the connection as
/// it exits.
async fn ask_to_stop(rt: &rt::Rt, home: &Home, build: &str) -> Result<(), Error> {
    let mut conn = reach(rt, home, build).await?;
    conn.stream.send(&ClientMsg::Shutdown).await?;
    answered(rt, DRAIN_TIMEOUT, conn.stream.drained(&mut conn.buffered)).await
}

/// What the running daemon says of itself, if one runs and answers; never
/// starts one.
pub fn status(home: &Home) -> Result<Option<Status>, Error> {
    let Some(daemon) = live_daemon(home) else {
        return Ok(None);
    };
    status_of(home, &daemon)
        .map(Some)
        .inspect_err(|e| note_timeout(home, &daemon, e))
}

pub(crate) fn note_timeout(home: &Home, daemon: &Record, error: &Error) {
    if matches!(error, Error::NotResponding) {
        ext_logging::witness(home, daemon, ext_logging::Observation::Unresponsive);
    }
}

fn status_of(home: &Home, daemon: &Record) -> Result<Status, Error> {
    rt::block_on(async {
        let mut conn = reach(&link::BLOCKING, home, &daemon.build).await?;
        conn.stream.send(&ClientMsg::Status).await?;
        let mut pick = |msg| match msg {
            DaemonMsg::Status {
                pid,
                build,
                started,
                clients,
                known,
            } => Some(Status {
                pid,
                build,
                started,
                clients,
                sessions: known,
            }),
            _ => None,
        };
        answered(&link::BLOCKING, HELLO_TIMEOUT, async {
            Ok(until(&*conn.stream, &mut conn.buffered, &mut pick).await?)
        })
        .await
    })
}

/// What a daemon says of itself; see [`status`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub pid: u32,
    pub build: String,
    /// Seconds since the Unix epoch at start.
    pub started: u64,
    /// Connections open, this one included.
    pub clients: u32,
    pub sessions: u32,
}

/// Stops any running daemon and connects to a fresh one, which serves even
/// after the crash-loop breaker stopped the last ones; as `client`, if it
/// is named.
pub fn restart(
    home: &Home,
    staged: &Staged,
    client: Option<ClientId>,
) -> Result<Connection, Error> {
    Daemon::local(home.clone(), staged.clone())
        .open(client, Start::Replace, &|| {}, &|_| Ok(Vec::new()))
}

/// Serves `ssh <host> a bridge`: connects to this machine's daemon, prints
/// the `A-BRIDGE <token> env` sync line so the far side can skip whatever
/// the login shell printed, then this side's environment as a frame, then
/// pipes stdin and stdout to the socket verbatim. The far client attaches
/// with that environment, so commands run with this host's `PATH` in this
/// host's directories. The far client does its own hello. Ends when either
/// side closes: the daemon may keep a connection open after its client is
/// done.
pub fn bridge(home: &Home, token: &str) -> io::Result<()> {
    let (stream, _) = rt::block_on(connect_or_spawn(&link::BLOCKING, home, false))?;
    let env = ClientMsg::Environment(capture()?);
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{} {token} env", remote::SYNC)?;
    write_frame(&mut stdout, &env)?;
    let mut up = stream.try_clone()?;
    // thread-exception: bridge-stdin
    thread::spawn(move || {
        let _ = copy(io::stdin().lock(), &mut up);
        std::process::exit(0);
    });
    copy(&stream, stdout)
}

/// Copies until EOF, flushing each read: stdout is line-buffered and frames
/// are not lines.
fn copy(mut from: impl Read, mut to: impl Write) -> io::Result<()> {
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n])?;
        to.flush()?;
    }
}

/// All a daemon takes of the environment of the client that starts it:
/// enough for its own processes to find their tools and a temporary
/// directory. It serves every client, so nothing of one client's (keys,
/// `A_*` settings) may reach the others: what a session runs with comes
/// from its own client's attach, and the daemon's settings from
/// `config.toml`.
const DAEMON_ENV: &[&str] = &[
    "XDG_RUNTIME_DIR",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
];

fn open(path: &Path) -> io::Result<File> {
    File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

fn wedged(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("{what} holds its lock but the daemon is not listening"),
    )
}

/// `start.lock`: one starter at a time, whether a client or the OS at login.
async fn starting(rt: &rt::Rt, home: &Home) -> io::Result<File> {
    sys::fs::private_dir(&home.run_dir())?;
    locked(
        rt,
        open(&home.run_dir().join("start.lock"))?,
        Some(HELLO_TIMEOUT),
    )
    .await
    .ok_or_else(|| wedged("another client"))
}

/// The daemon lock and endpoint for a daemon the OS starts at login, taken
/// in the order a self-spawning client takes them and under the same
/// `start.lock`, so the two never race. `None` when a daemon already serves:
/// once this is bound, clients connect into its backlog.
pub fn bind_at_login(home: &Home) -> io::Result<Option<(sys::DaemonLock, sys::Listener)>> {
    rt::block_on(bind_at_login_async(&link::BLOCKING, home))
}

async fn bind_at_login_async(
    rt: &rt::Rt,
    home: &Home,
) -> io::Result<Option<(sys::DaemonLock, sys::Listener)>> {
    let _starting = starting(rt, home).await?;
    if local_connect(rt, home).await.is_ok() {
        return Ok(None);
    }
    // A daemon draining or stopping still holds the record and its endpoint;
    // binding before it lets go would replace the discovery it withdraws.
    let lock = locked(rt, open(&home.daemon_pid_file())?, Some(HELLO_TIMEOUT))
        .await
        .ok_or_else(|| wedged("the daemon"))?;
    let listener = home.bind()?;
    Ok(Some((sys::adopt_lock(lock, home)?, listener)))
}

/// A login entry whose host does not outlive it: start a detached daemon
/// as any client does, unless one serves.
pub fn spawn_at_login(home: &Home) -> io::Result<()> {
    rt::block_on(connect_or_spawn(&link::BLOCKING, home, false)).map(drop)
}

/// Zero-lag start: when no daemon is listening, take the daemon lock, bind
/// the socket here, hand both to a detached daemon running
/// `runtime/current`, and connect straight into its backlog. The lock goes
/// with the fd, so it is held from here until the daemon exits, and only
/// that daemon reaps a dead predecessor or refuses to serve after a crash
/// loop (`reset` clears the loop first). Says whether it started that
/// daemon.
///
/// Nothing is polled. One client starts the daemon at a time, under
/// `start.lock`, and lets go once the socket is bound: the others wait on
/// that lock, then find the socket. A daemon lock held while no socket
/// listens is then an exiting daemon's (let go as it ends) or a wedged
/// one's, and is waited on for as long as a hello may take.
pub(crate) async fn connect_or_spawn(
    rt: &rt::Rt,
    home: &Home,
    reset: bool,
) -> io::Result<(OwnedConnection, bool)> {
    use std::process::{Command, Stdio};

    match local_connect(rt, home).await {
        Ok(stream) => return Ok((stream, false)),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Err(error),
        Err(_) => {}
    }

    let _starting = starting(rt, home).await?;
    match local_connect(rt, home).await {
        Ok(stream) => return Ok((stream, false)),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Err(error),
        Err(_) => {}
    }
    let lock = locked(rt, open(&home.daemon_pid_file())?, Some(HELLO_TIMEOUT))
        .await
        .ok_or_else(|| wedged("the daemon"))?;

    // The daemon leaves this process's session (`setsid`) as it starts, so
    // it outlives this client, its terminal and its app, and opens its own
    // log. Reaped here should it exit first, so a long-lived client (the
    // app) collects no zombies.
    let inherited = DAEMON_ENV
        .iter()
        .filter_map(|&name| Some((name, std::env::var_os(name)?)));
    let mut cmd = Command::new(home.runtime().current_exe());
    cmd.arg("__daemon")
        .args(reset.then_some("--reset-breaker"))
        .env_clear()
        .envs(inherited)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    home.daemon_env(&mut cmd);
    let spawning = home.clone();
    let daemon = rt::unblock_with(&**rt, rt::BlockingClass::Storage, 64 << 10, move || {
        sys::spawn_daemon(&mut cmd, &spawning, lock)
    })
    .await?;
    rt::reap(&**rt, daemon);
    Ok((local_connect(rt, home).await?, true))
}

impl Event {
    /// Encoded payload plus fixed queue overhead; no payload copy is retained.
    pub fn retained_bytes(&self) -> usize {
        let mut bytes = size_of::<Self>();
        match &self.kind {
            EventKind::Msg(msg) => {
                bytes += msg.retained_bytes();
            }
            EventKind::Connected { commands, .. } => {
                for command in commands {
                    bytes += command.name.len() + command.description.len();
                }
            }
            _ => {
                bytes += 4096;
            }
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::Link as _;
    use protocol::{PROTO, read_frame};
    use std::io::BufReader;

    #[cfg(unix)]
    #[test]
    fn a_login_start_takes_the_daemon_lock_before_it_binds() {
        let root = std::env::temp_dir().join(format!("a-login-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = Home::from(root.join("home"));
        sys::fs::private_dir(&home.run_dir()).unwrap();
        // A daemon draining for an upgrade still holds its record.
        let draining = open(&home.daemon_pid_file()).unwrap();
        sys::fs::try_lock(&draining).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let starter = home.clone();
        let login = thread::spawn(move || {
            let _ = tx.send(bind_at_login(&starter).map(|bound| bound.is_some()));
        });
        thread::sleep(HELLO_TIMEOUT / 4);
        assert!(
            !home.run_dir().join("daemon.endpoint").exists(),
            "bound while another daemon held the record"
        );
        assert!(home.endpoint().connect().is_err());
        drop(draining);
        assert!(rx.recv().unwrap().unwrap());
        login.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_older_client_is_pointed_at_the_newer_build_not_at_a_restart() {
        let root = std::env::temp_dir().join(format!("a-client-mismatch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let home = Home::from(root.join("home"));
        // Built `age` seconds after the epoch: a later build is newer.
        let build = |name: &str, age: u64| {
            let exe = root.join(name);
            fs::write(&exe, name).unwrap();
            File::options()
                .write(true)
                .open(&exe)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000 + age))
                .unwrap();
            home.runtime().stage(&exe).unwrap()
        };
        let old = build("old", 1);
        let new = build("new", 2);

        let theirs = PROTO + 1;
        let told = LocalLink::new(home.clone()).mismatch(&old, theirs);
        let current = home.runtime().current_exe();
        assert_eq!(
            told,
            Error::Mismatch {
                theirs,
                newer: Some(current.clone()),
                host: None
            }
        );
        let told = told.to_string();
        assert!(
            told.contains(&*current.to_string_lossy()) && !told.contains("restart"),
            "{told}"
        );
        let told = LocalLink::new(home.clone())
            .mismatch(&new, theirs)
            .to_string();
        assert!(told.contains("restart the daemon"), "{told}");
        fs::remove_dir_all(&root).unwrap();
    }

    fn scratch(name: &str) -> (PathBuf, Home) {
        let root = tempfile::Builder::new()
            .prefix(&format!("a-client-{name}-"))
            .tempdir()
            .unwrap()
            .keep();
        fs::create_dir_all(root.join("run")).unwrap();
        (root.clone(), Home::from(root))
    }

    #[test]
    fn stop_signals_only_the_process_that_recorded_itself() {
        let (root, home) = scratch("stop");
        let me = Record::current("b1");
        // Another process now under the recorded pid: this one, which must
        // not be signalled.
        let impostor = Record {
            token: Some(me.token.unwrap_or(0) + 1),
            ..me
        };
        fs::write(home.daemon_pid_file(), impostor.to_string()).unwrap();
        let held = File::open(home.daemon_pid_file()).unwrap();
        sys::fs::lock(&held).unwrap();
        let stopped = stop(&home);
        let pid = diagnose(&home).pid;
        drop(held);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(pid, Some(std::process::id()));
        if impostor.token.is_some() && sys::process_started(impostor.pid).is_some() {
            assert!(
                stopped.is_err(),
                "an unverified process identity was accepted"
            );
        }
    }

    #[test]
    fn timed_out_lock_waits_leave_storage_available_for_stop() {
        let (root, home) = scratch("cancelled-locks");
        let record = Record {
            pid: u32::MAX,
            ..Record::current("absent")
        };
        fs::write(home.daemon_pid_file(), record.to_string()).unwrap();
        let held = File::open(home.daemon_pid_file()).unwrap();
        sys::fs::lock(&held).unwrap();
        let mut config = rt::Config::for_cores(1);
        config.storage.workers = 1;
        config.storage.queue = 1;
        let runtime: rt::Rt = Arc::new(rt::Threads::configured(config));
        let stopped = rt::block_on(async {
            for _ in 0..3 {
                assert!(!wait_exited_async(&runtime, &home, Some(Duration::from_millis(20))).await);
            }
            rt::or(async { Some(stop_async(&runtime, &home).await) }, async {
                runtime.sleep(Duration::from_secs(1)).await;
                None
            })
            .await
        });
        drop(held);
        fs::remove_dir_all(root).unwrap();
        assert!(
            matches!(stopped, Some(Err(_))),
            "stop must reach its native identity check while the record stays locked"
        );
    }

    #[test]
    fn a_lock_let_go_after_the_wait_is_not_kept() {
        let (root, home) = scratch("lock-wait");
        let path = home.daemon_pid_file();
        fs::write(&path, "").unwrap();
        let held = File::open(&path).unwrap();
        sys::fs::lock(&held).unwrap();
        // Held throughout the wait: nothing is taken.
        let waited = File::open(&path).unwrap();
        assert!(
            rt::block_on(locked(
                &link::BLOCKING,
                waited,
                Some(Duration::from_millis(50))
            ))
            .is_none()
        );
        // A cancelled wait cannot acquire the lock after its caller leaves.
        drop(held);
        let taken = rt::block_on(locked(
            &link::BLOCKING,
            File::open(&path).unwrap(),
            Some(Duration::from_millis(500)),
        ));
        assert!(taken.is_some());
        drop(taken);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn status_timeouts_preserve_a_witness_during_hello_and_reply() {
        for welcomed in [false, true] {
            let (root, home) = scratch(if welcomed {
                "status-reply"
            } else {
                "status-hello"
            });
            let daemon = Record::current("1.2.3+0123456789abcdef");
            fs::write(home.daemon_pid_file(), daemon.to_string()).unwrap();
            let held = File::open(home.daemon_pid_file()).unwrap();
            sys::fs::lock(&held).unwrap();
            let listener = home.bind().unwrap();
            let (release, released) = std::sync::mpsc::channel();
            let peer = thread::spawn(move || {
                let stream = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                assert!(matches!(
                    read_frame::<ClientMsg>(&mut reader).unwrap(),
                    Some(ClientMsg::Hello { .. })
                ));
                if welcomed {
                    let mut writer = stream.try_clone().unwrap();
                    write_frame(
                        &mut writer,
                        &DaemonMsg::Welcome {
                            proto: PROTO,
                            build: "1.2.3+0123456789abcdef".into(),
                            caps: Caps::ALL,
                            machine: MachineId([0; 16]),
                            commands: Vec::new(),
                            queries: Vec::new(),
                            upgrade: None,
                        },
                    )
                    .unwrap();
                    assert!(matches!(
                        read_frame::<ClientMsg>(&mut reader).unwrap(),
                        Some(ClientMsg::Status)
                    ));
                }
                let _ = released.recv();
            });
            let result = status(&home);
            let observed = fs::read_to_string(home.run_dir().join("daemon.witness"));
            release.send(()).unwrap();
            peer.join().unwrap();
            drop(held);
            home.endpoint().remove();
            fs::remove_dir_all(root).unwrap();
            assert!(matches!(result, Err(Error::NotResponding)), "{result:?}");
            let observed: serde_json::Value = serde_json::from_str(&observed.unwrap()).unwrap();
            assert_eq!(observed["pid"], daemon.pid);
            assert_eq!(observed["started"], daemon.started);
            assert_eq!(observed["reason"], "unresponsive");
        }
    }
}
