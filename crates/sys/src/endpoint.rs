//! The daemon's local endpoint: a Unix socket in a private directory, or a
//! named pipe on Windows. One type for its address, one for the listener
//! and one for a connection, so nothing above `sys` names a platform's.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::proc::{Pass, Passed};
use crate::{Home, fnv1a};

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
    /// a pipe is `\\.\pipe\a-<hash of the home>`.
    pub fn endpoint(&self) -> Endpoint {
        let hash = fnv1a(self.root().as_os_str().as_encoded_bytes());
        Endpoint(imp::address(hash))
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
    pub fn adopt(passed: Passed) -> io::Result<Self> {
        imp::adopt(passed).map(Self)
    }
}

impl Pass for Listener {
    fn passed(&self) -> Passed {
        crate::proc::passed(&self.0)
    }
}

/// A connection to or from the daemon.
#[derive(Debug)]
pub struct Stream(imp::Stream);

impl Stream {
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

    pub fn address(hash: u64) -> PathBuf {
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

    pub fn accept(listener: &UnixListener) -> io::Result<UnixStream> {
        listener.accept().map(|(stream, _)| stream)
    }

    pub fn listener_nonblocking(listener: &UnixListener, nonblocking: bool) -> io::Result<()> {
        listener.set_nonblocking(nonblocking)
    }

    pub fn adopt(Passed(fd): Passed) -> io::Result<UnixListener> {
        // SAFETY: the parent (`a` itself) passes the fd number of a
        // listening socket it bound and left open for us; nothing else in
        // this process owns that fd. `local_addr` below rejects anything
        // that is not a Unix socket.
        let listener = unsafe { UnixListener::from_raw_fd(fd) };
        listener.local_addr()?;
        crate::proc::keep(listener.as_fd())?;
        Ok(listener)
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

/// A client opens the pipe as a file, which is all it takes; serving one
/// (`CreateNamedPipeW` with `FILE_FLAG_FIRST_PIPE_INSTANCE`, owner-only)
/// lands with the Windows daemon.
#[cfg(windows)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::io::OwnedHandle;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use crate::proc::Passed;

    pub type Listener = OwnedHandle;
    pub type Stream = File;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "a's daemon does not run on Windows yet",
        )
    }

    pub fn address(hash: u64) -> PathBuf {
        PathBuf::from(format!(r"\\.\pipe\a-{hash:016x}"))
    }

    pub fn connect(path: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(path)
    }

    /// A pipe goes with its last handle.
    pub fn remove(_path: &Path) {}

    pub fn bind(_pipe: &Path, _link: &Path) -> io::Result<OwnedHandle> {
        Err(unsupported())
    }

    pub fn accept(_listener: &OwnedHandle) -> io::Result<File> {
        Err(unsupported())
    }

    pub fn listener_nonblocking(_listener: &OwnedHandle, _nonblocking: bool) -> io::Result<()> {
        Err(unsupported())
    }

    pub fn adopt(_passed: Passed) -> io::Result<OwnedHandle> {
        Err(unsupported())
    }

    pub fn read_timeout(_stream: &File, _timeout: Option<Duration>) -> io::Result<()> {
        Err(unsupported())
    }

    pub fn stream_nonblocking(_stream: &File, _nonblocking: bool) -> io::Result<()> {
        Err(unsupported())
    }
}
