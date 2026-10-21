//! The daemon's local endpoint: a Unix socket in a private directory, or a
//! named pipe on Windows. One type for its address, one for the listener
//! and one for a connection, so nothing above `sys` names a platform's.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Home;
#[cfg(unix)]
use crate::proc::Pass;
use crate::proc::Passed;

/// Where a home's daemon listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint(PathBuf);

impl Endpoint {
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A connection to whoever listens here.
    pub fn connect(&self) -> io::Result<Stream> {
        imp::connect(&self.0).map(Stream)
    }

    /// Takes the endpoint away, so the next client starts a fresh daemon
    /// (one that raced into our backlog sees EOF and retries).
    pub fn remove(&self) {
        imp::remove(&self.0);
    }
}

impl Home {
    /// The daemon's endpoint. A socket path is kept short because
    /// `sun_path` holds ~104 bytes: `/tmp/a-<uid>/<hash of the home>.sock`;
    /// Windows discovers a process-generation pipe through the daemon record.
    pub fn endpoint(&self) -> Endpoint {
        let hash = imp::home_hash(self.root());
        Endpoint(imp::address(self.root(), hash))
    }

    /// Listens at [`Home::endpoint`], replacing a stale socket, and points
    /// [`Home::socket_link`] at it.
    pub fn bind(&self) -> io::Result<Listener> {
        imp::bind(&self.endpoint().0, &self.socket_link()).map(Listener)
    }
}

/// The daemon's end of its endpoint.
pub struct Listener(imp::Listener);

impl Listener {
    /// The next client's connection.
    pub fn accept(&self) -> io::Result<Stream> {
        imp::accept(&self.0).map(Stream)
    }

    /// Whether [`Listener::accept`] would-blocks instead of waiting.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        imp::listener_nonblocking(&self.0, nonblocking)
    }

    /// Adopts the listener our parent bound and passed down.
    pub fn adopt(passed: Passed, endpoint: &Endpoint) -> io::Result<Self> {
        imp::adopt(passed, &endpoint.0).map(Self)
    }
}

#[cfg(unix)]
impl Pass for Listener {
    fn passed(&self) -> Passed {
        crate::proc::passed(&self.0)
    }
}

/// A connection to or from the daemon.
#[derive(Debug)]
pub struct Stream(imp::Stream);

impl Stream {
    /// Ends this direction for every clone, including a blocked reader.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        imp::shutdown(&self.0, how)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    /// How long reads wait; `None` for ever. macOS refuses (`EINVAL`) once
    /// the peer has hung up; the next read says so.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        imp::read_timeout(&self.0, timeout)
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        imp::stream_nonblocking(&self.0, nonblocking)
    }
}

/// What the reactor registers: a stream or listener is waited on, never
/// blocked on, by the daemon's tasks.
#[cfg(unix)]
impl std::os::fd::AsFd for Stream {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for Listener {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(windows)]
impl crate::CompletionSource for Stream {
    fn completion_events(&self) -> io::Result<[std::os::windows::io::OwnedHandle; 2]> {
        self.0.completion_events()
    }
    fn nonblocking(&self) -> io::Result<()> {
        self.set_nonblocking(true)
    }
}

#[cfg(windows)]
impl crate::CompletionSource for Listener {
    fn completion_events(&self) -> io::Result<[std::os::windows::io::OwnedHandle; 2]> {
        self.0.completion_events()
    }
    fn nonblocking(&self) -> io::Result<()> {
        self.set_nonblocking(true)
    }
}

#[cfg(unix)]
impl From<std::os::unix::net::UnixStream> for Stream {
    fn from(stream: std::os::unix::net::UnixStream) -> Self {
        Self(stream)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.0).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.0).flush()
    }
}

#[cfg(unix)]
mod imp {
    use std::fs;
    use std::io;
    use std::os::fd::{AsFd, FromRawFd};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use rustix::process::getuid;

    use crate::proc::Passed;

    pub type Listener = UnixListener;
    pub type Stream = UnixStream;

    pub fn home_hash(root: &Path) -> u64 {
        crate::fnv1a(root.as_os_str().as_encoded_bytes())
    }

    pub fn address(_root: &Path, hash: u64) -> PathBuf {
        socket_dir().join(format!("{hash:016x}.sock"))
    }

    pub fn connect(path: &Path) -> io::Result<UnixStream> {
        UnixStream::connect(path)
    }

    pub fn remove(path: &Path) {
        let _ = fs::remove_file(path);
    }

    pub fn bind(socket: &Path, link: &Path) -> io::Result<UnixListener> {
        prepare_socket_dir()?;
        let _ = fs::remove_file(socket);
        let listener = UnixListener::bind(socket)?;
        if fs::read_link(link).ok().as_deref() != Some(socket) {
            let _ = fs::remove_file(link);
            // Only a convenience: a home on a filesystem without symlinks
            // still works.
            let _ = std::os::unix::fs::symlink(socket, link);
        }
        Ok(listener)
    }

    /// A connection the daemon writes to without blocking: its send buffer
    /// holds a whole write batch, so the writer waits on the socket once a
    /// batch rather than once each default 8 KiB macOS buffers.
    pub fn accept(listener: &UnixListener) -> io::Result<UnixStream> {
        let (stream, _) = listener.accept()?;
        // Only a speed-up: a socket that keeps its default still works.
        let _ = rustix::net::sockopt::set_socket_send_buffer_size(&stream, SEND_BUFFER);
        Ok(stream)
    }

    const SEND_BUFFER: usize = 256 << 10;

    pub fn listener_nonblocking(listener: &UnixListener, nonblocking: bool) -> io::Result<()> {
        listener.set_nonblocking(nonblocking)
    }

    pub fn adopt(Passed(fd): Passed, _endpoint: &Path) -> io::Result<UnixListener> {
        // SAFETY: the parent (`a` itself) passes the fd number of a
        // listening socket it bound and left open for us; nothing else in
        // this process owns that fd. `local_addr` below rejects anything
        // that is not a Unix socket.
        let listener = unsafe { UnixListener::from_raw_fd(fd) };
        listener.local_addr()?;
        crate::proc::keep(listener.as_fd())?;
        Ok(listener)
    }

    pub fn shutdown(stream: &UnixStream, how: std::net::Shutdown) -> io::Result<()> {
        stream.shutdown(how)
    }

    pub fn read_timeout(stream: &UnixStream, timeout: Option<Duration>) -> io::Result<()> {
        match stream.set_read_timeout(timeout) {
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => Ok(()),
            done => done,
        }
    }

    pub fn stream_nonblocking(stream: &UnixStream, nonblocking: bool) -> io::Result<()> {
        stream.set_nonblocking(nonblocking)
    }

    /// Creates the socket directory, or verifies an existing one is a real
    /// directory owned by us with mode 0700 (it lives in shared `/tmp`).
    fn prepare_socket_dir() -> io::Result<()> {
        let dir = socket_dir();
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let meta = fs::symlink_metadata(&dir)?;
        if !meta.is_dir() || meta.uid() != getuid().as_raw() || meta.mode() & 0o777 != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not a private directory owned by this user",
                    dir.display()
                ),
            ));
        }
        Ok(())
    }

    fn socket_dir() -> PathBuf {
        PathBuf::from(format!("/tmp/a-{}", getuid().as_raw()))
    }
}

#[cfg(windows)]
#[path = "endpoint_windows.rs"]
pub(crate) mod imp;

#[cfg(all(test, unix))]
mod tests {
    use std::io::{self, Write};

    use crate::Home;

    /// A client that reads slowly costs the daemon's writer one wait per
    /// batch it writes, not one per default socket buffer (8 KiB on macOS).
    #[test]
    fn an_accepted_connection_takes_a_write_batch_without_blocking() {
        let root = std::env::temp_dir().join(format!("a-endpoint-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let home = Home::from(root.clone());
        let listener = home.bind().unwrap();
        let _client = home.endpoint().connect().unwrap();
        let mut stream = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let (chunk, mut taken) = ([0u8; 4096], 0);
        loop {
            match stream.write(&chunk) {
                Ok(n) => taken += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        home.endpoint().remove();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(taken >= 64 << 10, "{taken} bytes before it would block");
    }
}
