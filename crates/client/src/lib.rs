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

use protocol::{ClientMsg, DaemonMsg, Environment, read_frame, write_frame};
use sys::{Home, Signal, Staged};

#[cfg(unix)]
use protocol::PROTO;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

/// The read half of a connection, over any byte stream.
pub struct Reader(BufReader<Box<dyn Read + Send>>);

/// The write half of a connection.
pub struct Writer(Box<dyn Write + Send>);

/// A daemon connection, split so reading and writing can run on their own
/// threads.
pub struct Connection {
    pub reader: Reader,
    pub writer: Writer,
    /// The far host's environment, as its bridge captured it; `None` for
    /// this machine's daemon.
    pub remote: Option<Environment>,
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
        while self.recv()?.is_some() {}
        Ok(())
    }
}

impl Writer {
    pub fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        write_frame(&mut self.0, msg)
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

/// How long to wait for a draining daemon: its grace plus a margin.
#[cfg(unix)]
const DRAIN_TIMEOUT: Duration = Duration::from_secs(75);

/// Connects as build `staged` and completes the hello. A daemon draining for
/// a newer build is waited out (`on_drain` says so first), then the next
/// attempt starts the new build. A local daemon that does not answer in time
/// fails with `ErrorKind::TimedOut`; see [`diagnose`] and [`restart`].
#[cfg(unix)]
pub fn connect(
    endpoint: &Endpoint,
    staged: &Staged,
    on_drain: &dyn Fn(),
) -> io::Result<Connection> {
    // A daemon exiting (idle) as we connect, or draining, gets one more try
    // each.
    for _ in 0..3 {
        let (stream, mut conn) = match endpoint {
            Endpoint::Local(home) => {
                let stream = connect_or_spawn(home)?;
                stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
                let conn = split(&stream)?;
                (Some(stream), conn)
            }
            Endpoint::Ssh(host) => (None, remote::open(host, staged)?),
        };
        match hello(&mut conn.reader, &mut conn.writer, &staged.id) {
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

/// The daemon and its transports are Unix only until the named pipe
/// transport lands.
#[cfg(not(unix))]
pub fn connect(
    _endpoint: &Endpoint,
    _staged: &Staged,
    _on_drain: &dyn Fn(),
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

#[cfg(unix)]
fn split(stream: &UnixStream) -> io::Result<Connection> {
    Ok(Connection {
        reader: Reader(BufReader::new(Box::new(stream.try_clone()?))),
        writer: Writer(Box::new(stream.try_clone()?)),
        remote: None,
    })
}

#[cfg(unix)]
enum Greeting {
    Welcome,
    Draining,
    Mismatch(u32),
}

/// Why a daemon speaking another protocol cannot serve this client, and
/// the fix. Staging never downgrades, so when a newer build is `current`,
/// restarting the daemon would only start that build again: this `a` is
/// the one to replace.
#[cfg(unix)]
fn mismatch(endpoint: &Endpoint, staged: &Staged, supported: u32) -> io::Error {
    let ours = &staged.id;
    let message = match endpoint {
        Endpoint::Local(home) => match home.runtime().current() {
            Some(current) if current != *ours => format!(
                "the daemon runs a newer a ({current}, protocol {supported}) than this one \
                 ({ours}, protocol {PROTO}); run {} instead, or update this a",
                home.runtime().current_exe().display()
            ),
            _ => format!(
                "the daemon speaks protocol {supported}, this a {PROTO}; restart the daemon"
            ),
        },
        Endpoint::Ssh(host) => format!(
            "the daemon on {host} speaks protocol {supported}, this a {PROTO}; \
             update a on one side"
        ),
    };
    io::Error::new(io::ErrorKind::Unsupported, message)
}

#[cfg(unix)]
fn hello(reader: &mut Reader, writer: &mut Writer, build: &str) -> io::Result<Greeting> {
    writer.send(&ClientMsg::Hello {
        proto: PROTO,
        build: build.to_owned(),
    })?;
    let reply = reader.recv().map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            io::Error::new(io::ErrorKind::TimedOut, "the daemon is not responding")
        }
        io::ErrorKind::ConnectionReset => io::ErrorKind::UnexpectedEof.into(),
        _ => e,
    })?;
    match reply {
        Some(DaemonMsg::Welcome { .. }) => Ok(Greeting::Welcome),
        Some(DaemonMsg::Draining) => Ok(Greeting::Draining),
        Some(DaemonMsg::Mismatch { supported }) => Ok(Greeting::Mismatch(supported)),
        _ => Err(io::ErrorKind::UnexpectedEof.into()),
    }
}

/// What a client can tell about the daemon without talking to it.
pub struct Diagnosis {
    /// The pid recorded by a daemon that still holds its lock.
    pub pid: Option<u32>,
    pub log: PathBuf,
}

impl Diagnosis {
    /// The last lines of the daemon's log.
    pub fn log_tail(&self) -> Vec<String> {
        const LINES: usize = 10;
        let text = fs::read_to_string(&self.log).unwrap_or_default();
        let mut tail: Vec<String> = text.lines().rev().take(LINES).map(str::to_owned).collect();
        tail.reverse();
        tail
    }
}

pub fn diagnose(home: &Home) -> Diagnosis {
    Diagnosis {
        pid: live_daemon(home),
        log: home.daemon_log(),
    }
}

/// The running daemon's pid. Only trusted while the daemon holds its lock:
/// if we can take the lock, the daemon is gone and the pid may be reused.
fn live_daemon(home: &Home) -> Option<u32> {
    let file = File::open(home.daemon_pid_file()).ok()?;
    if file.try_lock().is_ok() {
        return None;
    }
    fs::read_to_string(home.daemon_pid_file())
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Stops the daemon: SIGTERM, then SIGKILL if it still holds its lock after
/// `HELLO_TIMEOUT`. Returns whether a daemon was running.
pub fn stop(home: &Home) -> io::Result<bool> {
    let Some(pid) = live_daemon(home) else {
        return Ok(false);
    };
    sys::signal_process(pid, Signal::Terminate)?;
    let deadline = Instant::now() + HELLO_TIMEOUT;
    // The held lock proves `pid` is still the daemon, so signalling it again
    // is safe.
    while live_daemon(home) == Some(pid) {
        if Instant::now() >= deadline {
            sys::signal_process(pid, Signal::Kill)?;
        }
        thread::sleep(Duration::from_millis(20));
    }
    #[cfg(unix)]
    let _ = fs::remove_file(home.socket());
    Ok(true)
}

/// Stops any running daemon and connects to a fresh one.
pub fn restart(home: &Home, staged: &Staged) -> io::Result<Connection> {
    stop(home)?;
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
    let stream = connect_or_spawn(home)?;
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

/// Zero-lag start: when no daemon is listening, bind the socket here, hand the
/// listening fd to a detached daemon running `runtime/current`, and connect
/// straight into its backlog.
#[cfg(unix)]
fn connect_or_spawn(home: &Home) -> io::Result<UnixStream> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::process::{Command, Stdio};

    let socket = home.socket();
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok(stream);
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
        return Ok(stream);
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
    let mut daemon = Command::new(home.runtime().current_exe())
        .args(["__daemon", "--listen-fd", &fd.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()?;
    thread::spawn(move || daemon.wait());
    drop(listener);
    UnixStream::connect(&socket)
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

        let told = mismatch(&local, &old, PROTO + 1).to_string();
        let current = home.runtime().current_exe();
        assert!(
            told.contains(&new.id) && told.contains(&*current.to_string_lossy()),
            "{told}"
        );
        assert!(!told.contains("restart"), "{told}");
        let told = mismatch(&local, &new, PROTO + 1).to_string();
        assert!(told.contains("restart the daemon"), "{told}");
        fs::remove_dir_all(&root).unwrap();
    }
}
