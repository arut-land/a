//! Client side: reach a daemon and complete the hello. This machine's daemon
//! is reached over its Unix socket and spawned if needed; another machine's
//! through `ssh <host> a bridge`, whose stdio carries the same frames.

#[cfg(unix)]
mod remote;

use std::fs::{self, File};
use std::io::{self, BufReader, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use protocol::view::SessionList;
use protocol::{
    Cap, Caps, ClientDecl, ClientMsg, DaemonMsg, Environment, SessionSummary, Target, read_frame,
    write_frame,
};
use sys::crash::{Crashes, Record};
use sys::{Home, Signal, Staged};

#[cfg(unix)]
use protocol::{PROTO, Versions};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

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
}

/// What the daemon's welcome settled for a connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Welcome {
    pub proto: u32,
    /// The capabilities taken up: none from a daemon older than them.
    pub caps: Caps,
    /// This connection's id, as origins name it; 0 from a daemon older
    /// than them.
    pub client: u32,
}

impl Welcome {
    /// `text` for the running turn: a steer where the daemon takes them,
    /// else plain input, which waits for the turn to end. The first
    /// daemons drop a connection that sends them a steer.
    pub fn steer(&self, text: impl Into<String>) -> ClientMsg {
        let text = text.into();
        if self.caps.has(Cap::Steer) {
            ClientMsg::Steer {
                text,
                client_seq: None,
            }
        } else {
            ClientMsg::input(text)
        }
    }
}

impl Connection {
    /// The workspace a session attached over this connection works in and
    /// the environment its commands run with: this process's, or a remote
    /// daemon's bridge's, never this machine's paths on another.
    pub fn origin(&self) -> io::Result<(PathBuf, Environment)> {
        if let Some(env) = &self.remote {
            return Ok((env.cwd.clone(), env.clone()));
        }
        let env = capture()?;
        Ok((sys::workspace_of(&env.cwd), env))
    }

    /// The attach to `target` from this connection's [`origin`], replaying
    /// the entries after `since`; with the origin's environment when
    /// `with_env`, else the session keeps the one it has.
    ///
    /// [`origin`]: Connection::origin
    pub fn attach(&self, target: Target, since: u64, with_env: bool) -> io::Result<ClientMsg> {
        let (workspace, env) = self.origin()?;
        Ok(ClientMsg::Attach {
            target,
            workspace,
            since,
            env: with_env.then_some(env),
            decl: ClientDecl::default(),
        })
    }

    /// Sends each of `msgs`.
    pub fn send_all(&mut self, msgs: &[ClientMsg]) -> io::Result<()> {
        msgs.iter().try_for_each(|msg| self.writer.send(msg))
    }

    /// The daemon's sessions.
    pub fn sessions(mut self) -> io::Result<Vec<SessionSummary>> {
        self.writer.send(&ClientMsg::List)?;
        let mut list = SessionList::default();
        self.reader
            .until(|msg| list.fold(&msg).then(|| std::mem::take(&mut list.sessions)))
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    pub(crate) fn new(out: Box<dyn Write + Send>) -> Self {
        Self { out, seq: 0 }
    }

    /// Sends `msg`, numbering it first if it is a mutation without a
    /// number: a daemon that takes up [`protocol::Cap::Ack`] answers it by
    /// that number, which [`Writer::last_seq`] gives.
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
#[cfg(unix)]
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a draining daemon: its grace plus a margin.
#[cfg(unix)]
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
    connect_with(endpoint, staged, on_drain, &|_| Ok(Vec::new()))
}

/// [`connect`], sending the messages `first` makes for the connection (an
/// attach, say) right behind the hello rather than after the welcome, which
/// saves a round trip. A daemon that does not welcome the client acts on
/// none of them, so each retry sends them again.
#[cfg(unix)]
pub fn connect_with(
    endpoint: &Endpoint,
    staged: &Staged,
    on_drain: &dyn Fn(),
    first: &dyn Fn(&Connection) -> io::Result<Vec<ClientMsg>>,
) -> io::Result<Connection> {
    // A daemon exiting (idle) as we connect, or draining, gets one more try
    // each.
    for _ in 0..3 {
        let (stream, mut conn) = match endpoint {
            Endpoint::Local(home) => {
                let (stream, started) = connect_or_spawn(home)?;
                let timeout = if started {
                    START_TIMEOUT
                } else {
                    HELLO_TIMEOUT
                };
                read_timeout(&stream, Some(timeout))?;
                let conn = split(&stream)?;
                (Some(stream), conn)
            }
            Endpoint::Ssh(host) => (None, remote::open(host, staged)?),
        };
        let first = first(&conn)?;
        match hello(&mut conn, &staged.id, &first) {
            Ok(Greeting::Mismatch(supported)) => {
                return Err(mismatch(endpoint, staged, supported));
            }
            Ok(Greeting::Welcome) => {
                if let Some(stream) = &stream {
                    read_timeout(stream, None)?;
                }
                return Ok(conn);
            }
            Ok(Greeting::Draining) => {
                on_drain();
                if let Some(stream) = &stream {
                    read_timeout(stream, Some(DRAIN_TIMEOUT))?;
                }
                conn.reader.drained()?;
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("the daemon kept closing the connection"))
}

/// The daemon and its transports are Unix only until the named pipe
/// transport lands.
#[cfg(not(unix))]
pub fn connect_with(
    _endpoint: &Endpoint,
    _staged: &Staged,
    _on_drain: &dyn Fn(),
    _first: &dyn Fn(&Connection) -> io::Result<Vec<ClientMsg>>,
) -> io::Result<Connection> {
    Err(unsupported())
}

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "a's daemon does not run on this platform yet",
    )
}

/// Sets how long reads on `stream` wait. macOS refuses (`EINVAL`) once the
/// daemon has hung up, as a draining one does when it exits; the next read
/// says so.
#[cfg(unix)]
fn read_timeout(stream: &UnixStream, timeout: Option<Duration>) -> io::Result<()> {
    match stream.set_read_timeout(timeout) {
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => Ok(()),
        done => done,
    }
}

#[cfg(unix)]
fn split(stream: &UnixStream) -> io::Result<Connection> {
    Ok(Connection {
        reader: Reader(BufReader::new(Box::new(stream.try_clone()?))),
        writer: Writer::new(Box::new(stream.try_clone()?)),
        remote: None,
        welcome: Welcome::default(),
    })
}

#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
fn hello(conn: &mut Connection, build: &str, first: &[ClientMsg]) -> io::Result<Greeting> {
    conn.writer.send(&ClientMsg::Hello {
        proto: PROTO,
        build: build.to_owned(),
        min: Versions::OURS.min_field(),
        caps: Caps::ALL,
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
            ..
        }) => {
            sent?;
            conn.welcome = Welcome {
                proto,
                caps,
                client,
            };
            Ok(Greeting::Welcome)
        }
        Some(DaemonMsg::Draining) => Ok(Greeting::Draining),
        Some(DaemonMsg::Mismatch { supported, min }) => {
            Ok(Greeting::Mismatch(Versions::new(min, supported)))
        }
        _ => Err(io::ErrorKind::UnexpectedEof.into()),
    }
}

/// What a client can tell about the daemon without talking to it.
pub struct Diagnosis {
    /// The pid recorded by a daemon that still holds its lock.
    pub pid: Option<u32>,
    /// What that daemon recorded about itself.
    pub record: Option<Record>,
    pub log: PathBuf,
    /// Recent crashes, and whether clients have stopped respawning.
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

    /// What to tell a person about recent crashes, if anything.
    pub fn crash_note(&self) -> Option<String> {
        let last = self.crashes.last.as_ref()?;
        let summary = fs::read_to_string(last)
            .ok()
            .and_then(|text| text.lines().next().map(str::to_owned))
            .unwrap_or_default();
        let breaker = if self.crashes.tripped() {
            format!(
                "; it crashed {} times in a row soon after starting, so it is not restarted \
                 automatically (`a daemon restart` tries again)",
                self.crashes.young
            )
        } else {
            String::new()
        };
        Some(format!(
            "last crash: {summary} ({}){breaker}",
            last.display()
        ))
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

/// Stops the daemon: SIGTERM, then SIGKILL if it still holds its lock after
/// `HELLO_TIMEOUT`. Each signal goes only to the process the daemon
/// recorded, checked by its start time as well as its pid. Returns whether
/// a daemon was running.
pub fn stop(home: &Home) -> io::Result<bool> {
    let Some(daemon) = live_daemon(home) else {
        return Ok(false);
    };
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
    let deadline = Instant::now() + HELLO_TIMEOUT;
    let mut killed = false;
    while live_daemon(home).is_some_and(|d| d.pid == daemon.pid) {
        if Instant::now() >= deadline && !killed {
            signal(Signal::Kill)?;
            killed = true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    #[cfg(unix)]
    let _ = fs::remove_file(home.socket());
    // Stopped on purpose: the record it left is not a crash.
    if let Ok(file) = File::options().write(true).open(home.daemon_pid_file())
        && file.try_lock().is_ok()
    {
        file.set_len(0)?;
    }
    Ok(true)
}

/// Stops any running daemon and connects to a fresh one, even after the
/// crash-loop breaker stopped respawning it.
pub fn restart(home: &Home, staged: &Staged) -> io::Result<Connection> {
    stop(home)?;
    sys::crash::reset(home)?;
    connect(&Endpoint::Local(home.clone()), staged, &|| {})
}

/// Serves `ssh <host> a bridge`: connects to this machine's daemon, prints
/// the `A-BRIDGE <token> env` sync line so the far side can skip whatever
/// the login shell printed, then this side's environment as a frame, then
/// pipes stdin and stdout to the socket verbatim. The far client attaches
/// with that environment, so commands run with this host's `PATH` in this
/// host's directories. The far client does its own hello. Ends when either
/// side closes: the daemon may keep a connection open after its client is
/// done.
#[cfg(unix)]
pub fn bridge(home: &Home, token: &str) -> io::Result<()> {
    let (stream, _) = connect_or_spawn(home)?;
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

#[cfg(not(unix))]
pub fn bridge(_home: &Home, _token: &str) -> io::Result<()> {
    Err(unsupported())
}

/// Copies until EOF, flushing each read: stdout is line-buffered and frames
/// are not lines.
#[cfg(unix)]
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
#[cfg(unix)]
const DAEMON_ENV: &[&str] = &[
    "HOME", "USER", "LOGNAME", "SHELL", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
];

/// Zero-lag start: when no daemon is listening, bind the socket here, hand the
/// listening fd to a detached daemon running `runtime/current`, and connect
/// straight into its backlog. Says whether it started that daemon.
#[cfg(unix)]
fn connect_or_spawn(home: &Home) -> io::Result<(UnixStream, bool)> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::process::{Command, Stdio};

    let socket = home.socket();
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok((stream, false));
    }

    let run = home.run_dir();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&run)?;
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700))?;
    let lock = File::create(home.spawn_lock())?;
    lock.lock()?;
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok((stream, false));
    }
    sys::crash::reap_unlocked(home)?;
    let crashes = Crashes::of(home);
    if crashes.tripped() {
        let report = crashes
            .last
            .map_or_else(String::new, |p| format!("; see {}", p.display()));
        return Err(io::Error::other(format!(
            "the daemon crashed {} times in a row soon after starting{report}; \
             `a daemon restart` tries again",
            crashes.young
        )));
    }

    let listener = home.bind()?;
    let fd = sys::make_inheritable(&listener)?;

    let log_path = home.daemon_log();
    if let Some(dir) = log_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    // The daemon leaves this process's session (`setsid`) as it starts, so
    // it outlives this client, its terminal and its app. Reaped here should
    // it exit first, so a long-lived client (the app) collects no zombies.
    let inherited = DAEMON_ENV
        .iter()
        .filter_map(|&name| Some((name, std::env::var_os(name)?)));
    let mut daemon = Command::new(home.runtime().current_exe())
        .args(["__daemon", "--listen-fd", &fd.to_string()])
        .env_clear()
        .envs(inherited)
        .env("A_HOME", home.root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()?;
    thread::spawn(move || daemon.wait());
    drop(listener);
    Ok((UnixStream::connect(&socket)?, true))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_steer_is_sent_only_to_a_daemon_that_takes_it() {
        let steers = Welcome {
            caps: Caps::ALL,
            ..Welcome::default()
        };
        assert!(matches!(steers.steer("also"), ClientMsg::Steer { text, .. } if text == "also"));
        let first = Welcome::default();
        assert!(matches!(first.steer("also"), ClientMsg::Input { text, .. } if text == "also"));
    }

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
    fn a_tripped_breaker_stops_the_respawning() {
        let (root, home) = scratch("breaker");
        let dead = Record {
            pid: 999_997,
            ..Record::current("b1")
        };
        for _ in 0..sys::crash::LIMIT {
            fs::write(home.daemon_pid_file(), dead.to_string()).unwrap();
            sys::crash::reap_unlocked(&home).unwrap();
        }
        let told = connect_or_spawn(&home).unwrap_err().to_string();
        let note = diagnose(&home).crash_note().unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert!(
            told.contains("crashed 3 times") && told.contains("a daemon restart"),
            "{told}"
        );
        assert!(
            note.contains("pid 999997") && note.contains("not restarted"),
            "{note}"
        );
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
