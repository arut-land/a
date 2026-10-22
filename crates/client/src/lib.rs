//! The client SDK every app is a renderer over. Below, the transport:
//! reach a daemon and complete the hello (this machine's over its Unix
//! socket, spawned if needed; another machine's through `ssh <host> a
//! bridge`, whose stdio carries the same frames). Above it, without I/O, a
//! session's link, view and intents ([`SessionClient`]) and the one slash
//! command table ([`COMMANDS`]); a [`Driver`] owns transport on the supplied runtime.

mod approval;
mod command;
mod drive;
mod inbox;
#[cfg(test)]
mod one_rule;
mod remote;
mod session;
mod stream;
mod submission;
pub use stream::{ReplyEvent, ReplyStream, Sanitizer, sanitize};
pub use submission::{Submission, SubmissionId, SubmissionState};
mod watch;

pub use approval::{ApprovalPart, approval_parts, approval_text};
pub use command::{COMMANDS, Cmd, Command, help, parse};
pub use drive::{ClientHandle, Driver, Renderer, pump};
pub use inbox::{Inbox, PUMP_BATCH, Post, QueueBudget, inbox};
pub use session::{
    Attach, ConnId, Effect, End, Env, Event, EventKind, Io, Link, Notice, Offer, SessionClient,
    Show, TurnView,
};
pub use watch::SessionWatch;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use protocol::view::SessionList;
use protocol::{
    Caps, ClientDecl, ClientId, ClientMsg, CommandInfo, DaemonMsg, Environment, HomeSettings,
    PROTO, SessionId, SessionSummary, SettingsChange, Versions, read_frame, write_frame,
};
use sys::crash::{Crashes, Record};
use sys::{Home, Signal, Staged, Stream};

/// Why a daemon could not be reached, or stopped serving this client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The daemon speaks `theirs`, not this build's protocol. Staging never
    /// downgrades, so when a `newer` build is staged, restarting the daemon
    /// would only start it again: that build is the one to run. On another
    /// `host`, one side or the other is to be updated.
    Mismatch {
        theirs: Versions,
        newer: Option<PathBuf>,
        host: Option<String>,
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
    /// The socket, `ssh` or the spawn failed.
    Io(io::ErrorKind, String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mine = Versions::OURS;
        match self {
            Self::Mismatch {
                theirs,
                newer: Some(newer),
                ..
            } => write!(
                f,
                "the daemon runs a newer a ({theirs}) than this one ({mine}); run {} instead, or update this a",
                newer.display()
            ),
            Self::Mismatch {
                theirs,
                host: Some(host),
                ..
            } => write!(
                f,
                "the daemon on {host} speaks {theirs}, this a {mine}; update a on one side"
            ),
            Self::Mismatch { theirs, .. } => {
                write!(
                    f,
                    "the daemon speaks {theirs}, this a {mine}; restart the daemon"
                )
            }
            Self::Fork(reason) => reason.fmt(f),
            Self::NotResponding => f.write_str("the daemon is not responding"),
            Self::Refused(why) | Self::Io(_, why) => f.write_str(why),
            Self::Busy => f.write_str("the client transport is busy; try again"),
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
            Error::Closed => io::ErrorKind::UnexpectedEof,
            Error::Io(kind, _) => *kind,
        };
        Self::new(kind, e.to_string())
    }
}

/// The read half of a connection.
pub struct Reader(BufReader<Transport>);

/// The write half owns cancellation of its peer reader.
pub struct Writer {
    io: Option<Transport>,
    child: Option<sys::ContainedChild>,
}

enum Transport {
    Local(Stream),
    Pipe(sys::ProcessPipe),
}

// Pipe and socket types coincide on some targets.
#[allow(clippy::match_same_arms)]
impl Read for Transport {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Local(io) => io.read(bytes),
            Self::Pipe(io) => io.read(bytes),
        }
    }
}

#[allow(clippy::match_same_arms)]
impl Write for Transport {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Local(io) => io.write(bytes),
            Self::Pipe(io) => io.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Some(Transport::Local(stream)) = &self.io {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A daemon connection, split so reading and writing can make progress independently.
pub struct Connection {
    pub reader: Reader,
    pub writer: Writer,
    /// The far host's environment, as its bridge captured it; `None` for
    /// this machine's daemon.
    pub remote: Option<Environment>,
    /// What the hello settled.
    pub welcome: Welcome,
    /// The slash commands the daemon advertised.
    pub commands: Vec<CommandInfo>,
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
        match &self.remote {
            Some(env) => Ok(env.clone()),
            None => capture(),
        }
    }

    /// The message making `attach`. The daemon finds the workspace from
    /// the directory given, else from the origin's; an attach that keeps
    /// the session's environment captures nothing.
    pub fn attach(&self, attach: &Attach) -> io::Result<ClientMsg> {
        attaching(attach, || self.origin())
    }

    /// Sends each of `msgs`.
    pub fn send_all(&mut self, msgs: &[ClientMsg]) -> io::Result<()> {
        msgs.iter().try_for_each(|msg| self.writer.send(msg))
    }

    /// The home settings, after making `change` to them. The daemon is
    /// their one writer; a change it refuses (a bad variable name, a file
    /// it cannot parse) is `InvalidInput`, saying why.
    pub fn settings(mut self, change: Option<SettingsChange>) -> io::Result<HomeSettings> {
        self.writer.send(&ClientMsg::Settings { change })?;
        let (settings, error) = self.reader.until(|msg| match msg {
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
        self.writer.send(&ClientMsg::WatchSessions)
    }

    /// The daemon's sessions, newest first.
    pub fn sessions(mut self) -> io::Result<Vec<SessionSummary>> {
        self.writer.send(&ClientMsg::List)?;
        let mut list = SessionList::default();
        self.reader
            .until(|msg| list.fold(&msg).then(|| std::mem::take(&mut list.sessions)))
    }
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
    let workspace = attach
        .workspace
        .clone()
        .unwrap_or_else(|| env.as_ref().map_or_default(|env| env.cwd.clone()));
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
        std::env::current_dir()?,
        std::env::vars_os(),
        io::stdin().is_terminal(),
    ))
}

impl Reader {
    pub fn recv(&mut self) -> io::Result<Option<DaemonMsg>> {
        read_frame(&mut self.0)
    }

    /// The first message `pick` accepts; the rest are skipped.
    pub fn until<T>(&mut self, mut pick: impl FnMut(DaemonMsg) -> Option<T>) -> io::Result<T> {
        while let Some(msg) = self.recv()? {
            if let Some(found) = pick(msg) {
                return Ok(found);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            Error::Closed.to_string(),
        ))
    }

    /// Waits for a draining daemon to close the connection as it exits.
    fn drained(&mut self) -> io::Result<()> {
        loop {
            match self.recv() {
                Ok(Some(_)) => {}
                Ok(None) => return Ok(()),
                // It exited with what we sent behind the hello unread.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

impl Writer {
    /// Sends `msg` as it is: a mutation is numbered by the session client
    /// that made it, which keeps it for the daemon's answer.
    pub fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        msg.write_frame(self.io.as_mut().expect("owned writer"))
    }
}

/// Where a daemon is.
#[derive(Clone, Debug)]
pub enum Endpoint {
    /// This machine's, over its Unix socket.
    Local(Home),
    /// Another machine's, through `ssh <host> a bridge`.
    Ssh(String),
}

/// A daemon as a client reaches it: where it is, and this build (which a
/// connection may start, or install on a host that lacks `a`).
#[derive(Clone, Debug)]
pub struct Daemon {
    pub endpoint: Endpoint,
    pub staged: Staged,
}

impl Daemon {
    pub const fn local(home: Home, staged: Staged) -> Self {
        Self {
            endpoint: Endpoint::Local(home),
            staged,
        }
    }

    pub const fn ssh(host: String, staged: Staged) -> Self {
        Self {
            endpoint: Endpoint::Ssh(host),
            staged,
        }
    }

    /// The home, for what acts on this machine's daemon only.
    pub fn home(&self) -> io::Result<&Home> {
        match &self.endpoint {
            Endpoint::Local(home) => Ok(home),
            Endpoint::Ssh(host) => Err(io::Error::other(format!(
                "this command acts on this machine; run it on {host} instead"
            ))),
        }
    }

    /// Connects and completes the hello. A daemon draining for a newer
    /// build is waited out (`on_drain` says so first), then the next
    /// attempt starts the new build.
    pub fn connect(&self, on_drain: &dyn Fn()) -> Result<Connection, Error> {
        self.connect_as(None, on_drain, &|_| Ok(Vec::new()))
    }

    /// [`Daemon::connect`] as `client`, which names the client across its
    /// connections so a mutation it sends again is done once, sending the
    /// messages `first` makes for the connection (an attach, say) right
    /// behind the hello rather than after the welcome, which saves a round
    /// trip. A daemon that does not welcome the client acts on none of
    /// them, so each retry sends them again.
    pub fn connect_as(
        &self,
        client: Option<ClientId>,
        on_drain: &dyn Fn(),
        first: &dyn Fn(&Connection) -> io::Result<Vec<ClientMsg>>,
    ) -> Result<Connection, Error> {
        self.connect_inner(client, on_drain, first)
            .inspect_err(|e| {
                if matches!(e, Error::NotResponding)
                    && let Endpoint::Local(home) = &self.endpoint
                    && let Some(daemon) = live_daemon(home)
                {
                    note_timeout(home, &daemon, e);
                }
            })
    }

    fn connect_inner(
        &self,
        client: Option<ClientId>,
        on_drain: &dyn Fn(),
        first: &dyn Fn(&Connection) -> io::Result<Vec<ClientMsg>>,
    ) -> Result<Connection, Error> {
        // A daemon exiting (idle) as we connect, or draining, gets one more
        // try each.
        for _ in 0..3 {
            let (stream, mut conn) = match &self.endpoint {
                Endpoint::Local(home) => {
                    let (stream, started) = connect_or_spawn(home, false)?;
                    let timeout = if started {
                        START_TIMEOUT
                    } else {
                        HELLO_TIMEOUT
                    };
                    stream.set_read_timeout(Some(timeout))?;
                    let conn = split(&stream)?;
                    (Some(stream), conn)
                }
                Endpoint::Ssh(host) => (None, remote::open(host, &self.staged)?),
            };
            let first = first(&conn)?;
            match hello(&mut conn, &self.staged.id, client, &first) {
                Ok(Greeting::Mismatch(theirs)) => return Err(self.mismatch(theirs)),
                Ok(Greeting::Welcome) => {
                    if let Some(stream) = &stream {
                        stream.set_read_timeout(None)?;
                    }
                    return Ok(conn);
                }
                Ok(Greeting::Draining) => {
                    on_drain();
                    if let Some(stream) = &stream {
                        stream.set_read_timeout(Some(DRAIN_TIMEOUT))?;
                    }
                    conn.reader.drained()?;
                }
                Err(Error::Closed) => {}
                Err(e) => return Err(e),
            }
        }
        Err(Error::Closed)
    }

    /// The daemon's sessions, newest first.
    pub fn sessions(&self) -> Result<Vec<SessionSummary>, Error> {
        Ok(self.connect(&|| {})?.sessions()?)
    }

    /// Starts a session in the origin's working directory with its
    /// environment, as `a new` does, and leaves it running.
    pub fn new_session(&self) -> Result<SessionId, Error> {
        let new = Attach::new(protocol::Target::New, Env::Origin);
        let mut conn = self.connect_as(None, &|| {}, &|conn| Ok(vec![conn.attach(&new)?]))?;
        let session = conn.reader.until(|msg| match msg {
            DaemonMsg::Attached { session, .. } => Some(session),
            _ => None,
        })?;
        conn.writer.send(&ClientMsg::Detach)?;
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
        let mut conn = self.connect_as(None, &|| {}, &|conn| Ok(vec![conn.attach(&attach)?]))?;
        let session = conn.reader.until(|msg| match msg {
            DaemonMsg::Attached { session, .. } => Some(Ok(session)),
            DaemonMsg::ForkRejected { reason } => Some(Err(Error::Fork(reason))),
            DaemonMsg::Error { message } => Some(Err(Error::Refused(message))),
            _ => None,
        })??;
        conn.writer.send(&ClientMsg::Detach)?;
        Ok(session)
    }

    /// Stops any running local daemon and connects to a fresh one, which
    /// serves even after the crash-loop breaker stopped the last ones.
    pub fn restart(&self) -> Result<Connection, Error> {
        restart(self.home()?, &self.staged, None)
    }

    /// Why a daemon speaking `theirs` cannot serve this client.
    fn mismatch(&self, theirs: Versions) -> Error {
        let (newer, host) = match &self.endpoint {
            Endpoint::Local(home) => (
                home.runtime()
                    .current()
                    .filter(|current| *current != self.staged.id)
                    .map(|_| home.runtime().current_exe()),
                None,
            ),
            Endpoint::Ssh(host) => (None, Some(host.clone())),
        };
        Error::Mismatch {
            theirs,
            newer,
            host,
        }
    }
}

/// How long a daemon may take to answer the hello before it counts as wedged.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// [`HELLO_TIMEOUT`] for a daemon this client just started: the first run
/// of a newly staged build is scanned by macOS first, which takes half a
/// second on an idle machine and seconds on a busy one.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a draining daemon: its grace plus a margin.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(75);

fn split(stream: &Stream) -> io::Result<Connection> {
    Ok(Connection {
        reader: Reader(BufReader::new(Transport::Local(stream.try_clone()?))),
        writer: Writer {
            io: Some(Transport::Local(stream.try_clone()?)),
            child: None,
        },
        remote: None,
        welcome: Welcome::default(),
        commands: Vec::new(),
    })
}

enum Greeting {
    Welcome,
    Draining,
    /// The daemon's versions.
    Mismatch(Versions),
}

/// Sends the hello and `first` back to back, then reads the answer.
fn hello(
    conn: &mut Connection,
    build: &str,
    client: Option<ClientId>,
    first: &[ClientMsg],
) -> Result<Greeting, Error> {
    conn.writer.send(&ClientMsg::Hello {
        proto: PROTO,
        build: build.to_owned(),
        min: Versions::OURS.min_field(),
        caps: Caps::ALL,
        client,
    })?;
    // A daemon that refuses the hello may hang up before reading these, so
    // failing to send them matters only once welcomed.
    let sent = conn.send_all(first);
    match conn.reader.recv()? {
        Some(DaemonMsg::Welcome {
            proto,
            caps,
            commands,
            ..
        }) => {
            sent?;
            conn.welcome = Welcome { proto, caps };
            conn.commands = commands;
            Ok(Greeting::Welcome)
        }
        Some(DaemonMsg::Draining) => Ok(Greeting::Draining),
        Some(DaemonMsg::Mismatch { supported, min }) => {
            Ok(Greeting::Mismatch(Versions::new(min, supported)))
        }
        // A daemon refusing to serve at all, after a crash loop.
        Some(DaemonMsg::Error { message }) => Err(Error::Refused(message)),
        _ => Err(Error::Closed),
    }
}

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
fn live_daemon(home: &Home) -> Option<Record> {
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
    let Some(daemon) = live_daemon(home) else {
        return Ok(false);
    };
    if ask_to_stop(home, &daemon.build).is_ok() && wait_exited(home, Some(HELLO_TIMEOUT)) {
        return Ok(true);
    }
    let process = sys::RecordedProcess::open(&daemon)?;
    ext_logging::witness(home, &daemon, ext_logging::Observation::Terminate);
    process.signal(Signal::Terminate)?;
    if !wait_exited(home, Some(HELLO_TIMEOUT)) {
        ext_logging::witness(home, &daemon, ext_logging::Observation::Kill);
        process.signal(Signal::Kill)?;
        wait_exited(home, None);
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
/// `within` (or ever): the lock is waited on, not polled. What a test or a
/// supervisor waits on in place of polling the pid.
pub fn wait_exited(home: &Home, within: Option<Duration>) -> bool {
    let Ok(file) = File::open(home.daemon_pid_file()) else {
        return true;
    };
    locked(file, within).is_some()
}

/// Takes the lock on `file` once it is free, waiting no longer than
/// `within`: on a helper thread, since a file lock has no timed wait. A
/// lock taken after the wait is given up is let go at once.
fn locked(file: File, within: Option<Duration>) -> Option<File> {
    if sys::fs::try_lock(&file).is_ok() {
        return Some(file);
    }
    let (tx, rx) = inbox(QueueBudget::default(), |_| size_of::<File>());
    thread::spawn(move || {
        if sys::fs::lock(&file).is_ok() {
            // Dropped, and so unlocked, when no one waits any more.
            let _ = tx.send(file);
        }
    });
    match within {
        Some(within) => rx.recv_timeout(within),
        None => rx.recv(),
    }
}

/// Connects to the running daemon, as its own build `build` so as not to
/// set off a drain, without starting one.
fn reach(home: &Home, build: &str) -> Result<(Stream, Connection), Error> {
    let stream = home.endpoint().connect()?;
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let mut conn = split(&stream)?;
    match hello(&mut conn, build, None, &[])? {
        Greeting::Welcome => Ok((stream, conn)),
        _ => Err(Error::Refused("the daemon cannot be asked".to_owned())),
    }
}

/// Asks the daemon to stop, and waits for it to close the connection as
/// it exits.
fn ask_to_stop(home: &Home, build: &str) -> Result<(), Error> {
    let (stream, mut conn) = reach(home, build)?;
    conn.writer.send(&ClientMsg::Shutdown)?;
    stream.set_read_timeout(Some(DRAIN_TIMEOUT))?;
    Ok(conn.reader.drained()?)
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

fn note_timeout(home: &Home, daemon: &Record, error: &Error) {
    if matches!(error, Error::NotResponding) {
        ext_logging::witness(home, daemon, ext_logging::Observation::Unresponsive);
    }
}

fn status_of(home: &Home, daemon: &Record) -> Result<Status, Error> {
    let (_, mut conn) = reach(home, &daemon.build)?;
    conn.writer.send(&ClientMsg::Status)?;
    let status = conn.reader.until(|msg| match msg {
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
    })?;
    Ok(status)
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
    stop(home)?;
    drop(connect_or_spawn(home, true)?);
    Daemon::local(home.clone(), staged.clone()).connect_as(client, &|| {}, &|_| Ok(Vec::new()))
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
    let (stream, _) = connect_or_spawn(home, false)?;
    let env = ClientMsg::Environment(capture()?);
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{} {token} env", remote::SYNC)?;
    write_frame(&mut stdout, &env)?;
    let mut up = stream.try_clone()?;
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
    "HOME", "USER", "LOGNAME", "SHELL", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
];

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
fn connect_or_spawn(home: &Home, reset: bool) -> io::Result<(Stream, bool)> {
    use std::process::{Command, Stdio};

    let endpoint = home.endpoint();
    if let Ok(stream) = endpoint.connect() {
        return Ok((stream, false));
    }

    sys::fs::private_dir(&home.run_dir())?;
    let open = |name: PathBuf| {
        File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(name)
    };
    let wedged = |what: &str| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{what} holds its lock but the daemon is not listening"),
        )
    };
    let _starting = locked(
        open(home.run_dir().join("start.lock"))?,
        Some(HELLO_TIMEOUT),
    )
    .ok_or_else(|| wedged("another client"))?;
    if let Ok(stream) = endpoint.connect() {
        return Ok((stream, false));
    }
    let lock = locked(open(home.daemon_pid_file())?, Some(HELLO_TIMEOUT))
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
        .env("A_HOME", home.root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut daemon = sys::spawn_daemon(&mut cmd, home, lock)?;
    thread::spawn(move || daemon.wait());
    Ok((endpoint.connect()?, true))
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

        let theirs = Versions::from(PROTO + 1);
        let told = Daemon::local(home.clone(), old).mismatch(theirs);
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
        let told = Daemon::local(home.clone(), new)
            .mismatch(theirs)
            .to_string();
        assert!(told.contains("restart the daemon"), "{told}");
        fs::remove_dir_all(&root).unwrap();
    }

    fn scratch(name: &str) -> (PathBuf, Home) {
        let root = std::env::temp_dir().join(format!("a-client-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
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
            let e = stopped.unwrap_err().to_string();
            assert!(e.contains("no longer the daemon"), "{e}");
        }
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
        assert!(locked(waited, Some(Duration::from_millis(50))).is_none());
        // Let go afterwards: the helper that was still waiting takes it and
        // drops it, so the next taker is not kept out.
        drop(held);
        let taken = locked(File::open(&path).unwrap(), Some(Duration::from_millis(500)));
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
                            commands: Vec::new(),
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
