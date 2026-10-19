//! The client SDK every app is a renderer over. Below, the transport:
//! reach a daemon and complete the hello (this machine's over its Unix
//! socket, spawned if needed; another machine's through `ssh <host> a
//! bridge`, whose stdio carries the same frames). Above it, without I/O, a
//! session's link, view and intents ([`SessionClient`]) and the one slash
//! command table ([`COMMANDS`]); a thin [`Driver`] runs them on threads.

mod command;
mod drive;
mod inbox;
mod remote;
mod session;
mod watch;

pub use command::{COMMANDS, Cmd, Command, help, parse};
pub use drive::Driver;
pub use inbox::{Inbox, Post, inbox};
pub use session::{Attach, Effect, End, Env, Event, Link, SessionClient, TurnView};
pub use watch::SessionWatch;

use std::fs::{self, File};
use std::io::{self, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use protocol::view::SessionList;
use protocol::{
    Caps, ClientDecl, ClientId, ClientMsg, CommandInfo, DaemonMsg, Environment, HomeSettings,
    PROTO, SessionSummary, SettingsChange, Versions, read_frame, write_frame,
};
use sys::crash::{Crashes, Record};
use sys::{Home, Signal, Staged, Stream};

/// The read half of a connection, over any byte stream.
pub struct Reader(BufReader<Box<dyn Read + Send>>);

/// The write half of a connection. It numbers the mutations it sends.
pub struct Writer {
    out: Box<dyn Write + Send>,
    /// The `client_seq` of the last mutation sent.
    seq: u64,
}

/// A daemon connection, split so reading and writing can run on their own
/// threads.
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
    /// This connection's id, as origins name it.
    pub client: u32,
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
    /// the directory given, else from the origin's.
    pub fn attach(&self, attach: &Attach) -> io::Result<ClientMsg> {
        let (workspace, env) = match &attach.env {
            Env::Keep => {
                let dir = attach.workspace.clone();
                (dir.map_or_else(|| self.origin().map(|e| e.cwd), Ok)?, None)
            }
            Env::Origin => {
                let env = self.origin()?;
                (
                    attach.workspace.clone().unwrap_or_else(|| env.cwd.clone()),
                    Some(env),
                )
            }
            Env::Given(env) => (
                attach.workspace.clone().unwrap_or_else(|| env.cwd.clone()),
                Some(env.clone()),
            ),
        };
        Ok(ClientMsg::Attach {
            target: attach.target.clone(),
            workspace,
            since: attach.since,
            env,
            decl: ClientDecl::default(),
        })
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

    /// The daemon's sessions.
    pub fn sessions(mut self) -> io::Result<Vec<SessionSummary>> {
        self.writer.send(&ClientMsg::List)?;
        let mut list = SessionList::default();
        self.reader
            .until(|msg| list.fold(&msg).then(|| std::mem::take(&mut list.sessions)))
    }
}

/// The one of `sessions` whose id (as `id` finds it), in full or short
/// form, starts with `prefix`.
pub fn matching<'a, T>(
    sessions: &'a [T],
    id: impl Fn(&T) -> Option<protocol::SessionId>,
    prefix: &str,
) -> Result<&'a T, String> {
    let mut found = sessions.iter().filter(|s| {
        id(s).is_some_and(|id| {
            id.short().to_string().starts_with(prefix) || id.to_string().starts_with(prefix)
        })
    });
    match (found.next(), found.next()) {
        (Some(one), None) => Ok(one),
        (None, _) => Err(format!("no session matches {prefix}")),
        (Some(_), Some(_)) => Err(format!("{prefix} matches several sessions")),
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
        Err(io::Error::other("daemon closed the connection"))
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
    pub(crate) fn new(out: Box<dyn Write + Send>) -> Self {
        Self { out, seq: 0 }
    }

    /// Sends `msg`, numbering it first if it is a mutation without a
    /// number: the daemon answers it by that number, which
    /// [`Writer::last_seq`] gives.
    pub fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        let mut numbered = msg.clone();
        match numbered.client_seq_mut() {
            Some(seq @ None) => {
                self.seq += 1;
                *seq = Some(self.seq);
                write_frame(&mut self.out, &numbered)
            }
            _ => write_frame(&mut self.out, msg),
        }
    }

    /// The `client_seq` of the last mutation sent.
    pub const fn last_seq(&self) -> u64 {
        self.seq
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

/// How long a daemon may take to answer the hello before it counts as wedged.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// [`HELLO_TIMEOUT`] for a daemon this client just started: the first run
/// of a newly staged build is scanned by macOS first, which takes half a
/// second on an idle machine and seconds on a busy one.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a draining daemon: its grace plus a margin.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(75);

/// Connects as build `staged` and completes the hello. A daemon draining for
/// a newer build is waited out (`on_drain` says so first), then the next
/// attempt starts the new build. A local daemon that does not answer in time
/// fails with `ErrorKind::TimedOut`; see [`diagnose`] and [`restart`].
pub fn connect(
    endpoint: &Endpoint,
    staged: &Staged,
    on_drain: &dyn Fn(),
) -> io::Result<Connection> {
    connect_with(endpoint, staged, None, on_drain, &|_| Ok(Vec::new()))
}

/// [`connect`], sending the messages `first` makes for the connection (an
/// attach, say) right behind the hello rather than after the welcome, which
/// saves a round trip. A daemon that does not welcome the client acts on
/// none of them, so each retry sends them again. `client` names the client
/// across its connections, so a mutation it sends again is done once.
pub fn connect_with(
    endpoint: &Endpoint,
    staged: &Staged,
    client: Option<ClientId>,
    on_drain: &dyn Fn(),
    first: &dyn Fn(&Connection) -> io::Result<Vec<ClientMsg>>,
) -> io::Result<Connection> {
    // A daemon exiting (idle) as we connect, or draining, gets one more try
    // each.
    for _ in 0..3 {
        let (stream, mut conn) = match endpoint {
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
            Endpoint::Ssh(host) => (None, remote::open(host, staged)?),
        };
        let first = first(&conn)?;
        match hello(&mut conn, &staged.id, client, &first) {
            Ok(Greeting::Mismatch(supported)) => {
                return Err(mismatch(endpoint, staged, supported));
            }
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
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("the daemon kept closing the connection"))
}

fn split(stream: &Stream) -> io::Result<Connection> {
    Ok(Connection {
        reader: Reader(BufReader::new(Box::new(stream.try_clone()?))),
        writer: Writer::new(Box::new(stream.try_clone()?)),
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

/// Why a daemon speaking another protocol cannot serve this client, and
/// the fix. Staging never downgrades, so when a newer build is `current`,
/// restarting the daemon would only start that build again: this `a` is
/// the one to replace.
fn mismatch(endpoint: &Endpoint, staged: &Staged, supported: Versions) -> io::Error {
    let ours = &staged.id;
    let mine = Versions::OURS;
    let message = match endpoint {
        Endpoint::Local(home) => match home.runtime().current() {
            Some(current) if current != *ours => format!(
                "the daemon runs a newer a ({current}, {supported}) than this one \
                 ({ours}, {mine}); run {} instead, or update this a",
                home.runtime().current_exe().display()
            ),
            _ => format!("the daemon speaks {supported}, this a {mine}; restart the daemon"),
        },
        Endpoint::Ssh(host) => {
            format!("the daemon on {host} speaks {supported}, this a {mine}; update a on one side")
        }
    };
    io::Error::new(io::ErrorKind::Unsupported, message)
}

/// Sends the hello and `first` back to back, then reads the answer.
fn hello(
    conn: &mut Connection,
    build: &str,
    client: Option<ClientId>,
    first: &[ClientMsg],
) -> io::Result<Greeting> {
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
    let reply = conn.reader.recv().map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            io::Error::new(io::ErrorKind::TimedOut, "the daemon is not responding")
        }
        io::ErrorKind::ConnectionReset => io::ErrorKind::UnexpectedEof.into(),
        _ => e,
    })?;
    match reply {
        Some(DaemonMsg::Welcome {
            proto,
            caps,
            client,
            commands,
            ..
        }) => {
            sent?;
            conn.welcome = Welcome {
                proto,
                caps,
                client,
            };
            conn.commands = commands;
            Ok(Greeting::Welcome)
        }
        Some(DaemonMsg::Draining) => Ok(Greeting::Draining),
        Some(DaemonMsg::Mismatch { supported, min }) => {
            Ok(Greeting::Mismatch(Versions::new(min, supported)))
        }
        // A daemon refusing to serve at all, after a crash loop.
        Some(DaemonMsg::Error { message }) => Err(io::Error::other(message)),
        _ => Err(io::ErrorKind::UnexpectedEof.into()),
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
    if file.try_lock().is_ok() {
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
    if ask_to_stop(home, &daemon.build).is_ok() && exited(home, Some(HELLO_TIMEOUT)) {
        return Ok(true);
    }
    let signal = |signal| {
        if !daemon.is_live() {
            return Err(io::Error::other(format!(
                "pid {} is no longer the daemon that recorded it; not signalling it",
                daemon.pid
            )));
        }
        sys::signal_process(daemon.pid, signal)
    };
    signal(Signal::Terminate)?;
    if !exited(home, Some(HELLO_TIMEOUT)) {
        signal(Signal::Kill)?;
        exited(home, None);
    }
    home.endpoint().remove();
    // Stopped on purpose: the record it left is not a crash.
    if let Ok(file) = File::options().write(true).open(home.daemon_pid_file())
        && file.try_lock().is_ok()
    {
        file.set_len(0)?;
    }
    Ok(true)
}

/// Whether the daemon let go of its lock, as it does when it exits, within
/// `within` (or ever): the lock is waited on, not polled.
fn exited(home: &Home, within: Option<Duration>) -> bool {
    let Ok(file) = File::open(home.daemon_pid_file()) else {
        return true;
    };
    let (tx, rx) = inbox();
    thread::spawn(move || {
        // Taken only to learn that it is free.
        let _ = tx.send(file.lock().is_ok());
    });
    match within {
        Some(within) => rx.recv_timeout(within),
        None => rx.recv(),
    }
    .unwrap_or(false)
}

/// Connects to the running daemon, as its own build `build` so as not to
/// set off a drain, without starting one.
fn reach(home: &Home, build: &str) -> io::Result<(Stream, Connection)> {
    let stream = home.endpoint().connect()?;
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let mut conn = split(&stream)?;
    match hello(&mut conn, build, None, &[])? {
        Greeting::Welcome => Ok((stream, conn)),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the daemon cannot be asked",
        )),
    }
}

/// Asks the daemon to stop, and waits for it to close the connection as
/// it exits.
fn ask_to_stop(home: &Home, build: &str) -> io::Result<()> {
    let (stream, mut conn) = reach(home, build)?;
    conn.writer.send(&ClientMsg::Shutdown)?;
    stream.set_read_timeout(Some(DRAIN_TIMEOUT))?;
    conn.reader.drained()
}

/// What the running daemon says of itself, if one runs and answers; never
/// starts one.
pub fn status(home: &Home) -> io::Result<Option<Status>> {
    let Some(daemon) = live_daemon(home) else {
        return Ok(None);
    };
    let (_, mut conn) = reach(home, &daemon.build)?;
    conn.writer.send(&ClientMsg::Status)?;
    conn.reader
        .until(|msg| match msg {
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
        })
        .map(Some)
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
pub fn restart(home: &Home, staged: &Staged, client: Option<ClientId>) -> io::Result<Connection> {
    stop(home)?;
    drop(connect_or_spawn(home, true)?);
    let endpoint = Endpoint::Local(home.clone());
    connect_with(&endpoint, staged, client, &|| {}, &|_| Ok(Vec::new()))
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
        let _ = pump(io::stdin().lock(), &mut up);
        std::process::exit(0);
    });
    pump(&stream, stdout)
}

/// Copies until EOF, flushing each read: stdout is line-buffered and frames
/// are not lines.
fn pump(mut from: impl Read, mut to: impl Write) -> io::Result<()> {
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
fn connect_or_spawn(home: &Home, reset: bool) -> io::Result<(Stream, bool)> {
    use std::process::{Command, Stdio};
    use sys::Pass;

    let endpoint = home.endpoint();
    if let Ok(stream) = endpoint.connect() {
        return Ok((stream, false));
    }

    sys::fs::private_dir(&home.run_dir())?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.daemon_pid_file())?;
    // Held: a daemon is starting, and binds before it does, or exiting, and
    // unlinks the socket first. Only one wedged between the two keeps it.
    let deadline = Instant::now() + HELLO_TIMEOUT;
    while let Err(held) = lock.try_lock() {
        if let fs::TryLockError::Error(e) = held {
            return Err(e);
        }
        if let Ok(stream) = endpoint.connect() {
            return Ok((stream, false));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the daemon holds its lock but is not listening",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }

    let listener = home.bind()?;
    let (listen_fd, lock_fd) = (listener.passed(), lock.passed());
    // The daemon leaves this process's session (`setsid`) as it starts, so
    // it outlives this client, its terminal and its app, and opens its own
    // log. Reaped here should it exit first, so a long-lived client (the
    // app) collects no zombies.
    let inherited = DAEMON_ENV
        .iter()
        .filter_map(|&name| Some((name, std::env::var_os(name)?)));
    let mut cmd = Command::new(home.runtime().current_exe());
    cmd.args(["__daemon", "--listen-fd", &listen_fd.to_string()])
        .args(["--lock-fd", &lock_fd.to_string()])
        .args(reset.then_some("--reset-breaker"))
        .env_clear()
        .envs(inherited)
        .env("A_HOME", home.root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    sys::pass(&mut cmd, [listen_fd, lock_fd])?;
    let mut daemon = cmd.spawn()?;
    thread::spawn(move || daemon.wait());
    drop((listener, lock));
    Ok((endpoint.connect()?, true))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn an_older_client_is_pointed_at_the_newer_build_not_at_a_restart() {
        let root = std::env::temp_dir().join(format!("a-client-mismatch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let home = Home::from(root.join("home"));
        let build = |name: &str| {
            let exe = root.join(name);
            fs::write(&exe, name).unwrap();
            home.runtime().stage(&exe).unwrap()
        };
        let old = build("old");
        // Built later, so newer.
        thread::sleep(Duration::from_millis(20));
        let new = build("new");
        let local = Endpoint::Local(home.clone());

        let told = mismatch(&local, &old, (PROTO + 1).into()).to_string();
        let current = home.runtime().current_exe();
        assert!(
            told.contains(&new.id) && told.contains(&*current.to_string_lossy()),
            "{told}"
        );
        assert!(!told.contains("restart"), "{told}");
        let told = mismatch(&local, &new, (PROTO + 1).into()).to_string();
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
        held.lock().unwrap();
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
}
