//! Client side: connect to the per-user daemon, spawning it if needed.

use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use protocol::{BUILD, ClientMsg, DaemonMsg, PROTO, read_frame, write_frame};
use sys::{Home, Signal};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(unix)]
pub struct Reader(BufReader<UnixStream>);

#[cfg(unix)]
pub struct Writer(UnixStream);

/// A daemon connection, split so reading and writing can run on their own
/// threads.
#[cfg(unix)]
pub type Connection = (Reader, Writer);

#[cfg(unix)]
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
}

#[cfg(unix)]
impl Writer {
    pub fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        write_frame(&mut self.0, msg)
    }
}

/// How long a daemon may take to answer the hello before it counts as wedged.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// Connects to the daemon (spawning it when absent) and completes the hello.
/// A daemon that does not answer in time fails with `ErrorKind::TimedOut`;
/// see [`diagnose`] and [`restart`].
#[cfg(unix)]
pub fn connect(home: &Home) -> io::Result<Connection> {
    match hello(connect_or_spawn(home)?) {
        // The daemon was exiting (idle) as we connected: start a fresh one.
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => hello(connect_or_spawn(home)?),
        result => result,
    }
}

#[cfg(unix)]
fn hello(stream: UnixStream) -> io::Result<Connection> {
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let mut reader = Reader(BufReader::new(stream.try_clone()?));
    let mut writer = Writer(stream);
    writer.send(&ClientMsg::Hello {
        proto: PROTO,
        build: BUILD.to_owned(),
    })?;
    let reply = reader.recv().map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            io::Error::new(io::ErrorKind::TimedOut, "the daemon is not responding")
        }
        io::ErrorKind::ConnectionReset => io::ErrorKind::UnexpectedEof.into(),
        _ => e,
    })?;
    writer.0.set_read_timeout(None)?;
    match reply {
        Some(DaemonMsg::Welcome { .. }) => Ok((reader, writer)),
        Some(DaemonMsg::Mismatch { supported }) => Err(io::Error::other(format!(
            "daemon speaks protocol {supported}, this client {PROTO}; restart the daemon"
        ))),
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
    let _ = fs::remove_file(home.socket());
    Ok(true)
}

/// Stops any running daemon and connects to a fresh one.
#[cfg(unix)]
pub fn restart(home: &Home) -> io::Result<Connection> {
    stop(home)?;
    connect(home)
}

/// Zero-lag start: when no daemon is listening, bind the socket here, hand the
/// listening fd to a detached daemon, and connect straight into its backlog.
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
    Command::new(std::env::current_exe()?)
        .args(["__daemon", "--listen-fd", &fd.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()?;
    drop(listener);
    UnixStream::connect(&socket)
}
