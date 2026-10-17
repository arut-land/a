//! Client side: connect to the per-user daemon, spawning it if needed.

use std::io::{self, BufReader};

use protocol::{BUILD, ClientMsg, DaemonMsg, PROTO, read_frame, write_frame};
use sys::Home;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(unix)]
pub struct Reader(BufReader<UnixStream>);

#[cfg(unix)]
pub struct Writer(UnixStream);

#[cfg(unix)]
impl Reader {
    pub fn recv(&mut self) -> io::Result<Option<DaemonMsg>> {
        read_frame(&mut self.0)
    }
}

#[cfg(unix)]
impl Writer {
    pub fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        write_frame(&mut self.0, msg)
    }
}

/// Connects to the daemon (spawning it when absent) and completes the hello.
#[cfg(unix)]
pub fn connect(home: &Home) -> io::Result<(Reader, Writer)> {
    let stream = connect_or_spawn(home)?;
    let mut reader = Reader(BufReader::new(stream.try_clone()?));
    let mut writer = Writer(stream);
    writer.send(&ClientMsg::Hello {
        proto: PROTO,
        build: BUILD.to_owned(),
    })?;
    match reader.recv()? {
        Some(DaemonMsg::Welcome { .. }) => Ok((reader, writer)),
        Some(DaemonMsg::Mismatch { supported }) => Err(io::Error::other(format!(
            "daemon speaks protocol {supported}, this client {PROTO}; restart the daemon"
        ))),
        _ => Err(io::Error::other(
            "daemon closed the connection during hello",
        )),
    }
}

/// Zero-lag start: when no daemon is listening, bind the socket here, hand the
/// listening fd to a detached daemon, and connect straight into its backlog.
#[cfg(unix)]
fn connect_or_spawn(home: &Home) -> io::Result<UnixStream> {
    use std::fs::{self, File, OpenOptions};
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
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
    home.prepare_socket_dir()?;
    let lock = File::create(home.spawn_lock())?;
    lock.lock()?;
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok(stream);
    }

    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
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
