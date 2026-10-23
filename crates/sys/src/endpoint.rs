//! The daemon's local endpoint: a Unix socket in a private directory, or a
//! named pipe on Windows. One type for its address, one for the listener
//! and one for a verified connection, so nothing above `sys` names a platform's.
//!
//! A listener claims its address before binding or removing a stale socket.
//! Unix keeps a locked, persistent sibling file through inherited startup;
//! Linux uses a validated XDG runtime directory, an existing private
//! `/run/user/UID`, or a private `/tmp/a-UID` fallback. Pathname sockets retain
//! filesystem access control. macOS uses the private `/tmp/a-UID` directory.
//! Windows claims the home with an exclusive file handle before creating a
//! first-instance, local-only pipe with a user-SID-only DACL. Accepted and
//! connected peers are checked before constructing `OwnedConnection`.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Home;
#[cfg(unix)]
use crate::proc::{Pass, Passed};

/// Where a home's daemon listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint(PathBuf);

impl Endpoint {
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// A connection to whoever listens here.
    pub fn connect(&self) -> io::Result<OwnedConnection> {
        imp::connect(&self.0).map(|stream| OwnedConnection(Stream(stream)))
    }

    /// Removes a stale endpoint only when no listener holds its claim.
    pub fn remove(&self) {
        imp::remove(&self.0);
    }
}

impl Home {
    /// The daemon's endpoint. Unix uses a short socket name in a private
    /// runtime directory; Windows discovers a process-generation pipe
    /// through the daemon record.
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
    pub fn accept(&self) -> io::Result<OwnedConnection> {
        imp::accept(&self.0).map(|stream| OwnedConnection(Stream(stream)))
    }

    /// Whether [`Listener::accept`] would-blocks instead of waiting.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        imp::listener_nonblocking(&self.0, nonblocking)
    }

    #[cfg(unix)]
    pub(crate) fn claim_passed(&self) -> Passed {
        crate::proc::passed(&self.0.claim)
    }

    /// Takes the one capability that can withdraw this listener's address.
    /// Retire it after session shutdown and before releasing the daemon record.
    pub fn retirement(&self) -> io::Result<EndpointRetirement> {
        imp::retirement(&self.0).map(EndpointRetirement)
    }

    /// Adopts the listener and claim our parent bound and passed down.
    pub fn adopt(file: std::fs::File, endpoint: &Endpoint) -> io::Result<Self> {
        imp::adopt(file, &endpoint.0).map(Self)
    }
}

#[cfg(unix)]
impl Pass for Listener {
    fn passed(&self) -> Passed {
        crate::proc::passed(&self.0)
    }
}

/// The listener owner's one-shot authority to unpublish and release its claim.
pub struct EndpointRetirement(imp::Retirement);

impl EndpointRetirement {
    pub fn retire(self) -> io::Result<()> {
        imp::retire(self.0)
    }
}

/// A local connection whose peer was verified before any protocol I/O.
#[derive(Debug)]
pub struct OwnedConnection(Stream);

impl OwnedConnection {
    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }
}

impl std::ops::Deref for OwnedConnection {
    type Target = Stream;

    fn deref(&self) -> &Stream {
        &self.0
    }
}

#[cfg(unix)]
impl TryFrom<std::os::unix::net::UnixStream> for OwnedConnection {
    type Error = io::Error;

    fn try_from(stream: std::os::unix::net::UnixStream) -> io::Result<Self> {
        imp::verify_peer(&stream)?;
        Ok(Self(Stream(stream)))
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for OwnedConnection {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(windows)]
impl crate::CompletionSource for OwnedConnection {
    fn completion_events(&self) -> io::Result<[std::os::windows::io::OwnedHandle; 2]> {
        self.0.completion_events()
    }
    fn nonblocking(&self) -> io::Result<()> {
        self.0.nonblocking()
    }
    fn complete_write(&self) -> io::Result<()> {
        self.0.complete_write()
    }
}

impl Read for OwnedConnection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Read for &OwnedConnection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(buf)
    }
}

impl Write for OwnedConnection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Write for &OwnedConnection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&self.0).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&self.0).flush()
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
    fn complete_write(&self) -> io::Result<()> {
        (&*self).flush()
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
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use rustix::process::geteuid;

    pub struct Listener {
        socket: UnixListener,
        pub(super) claim: fs::File,
        retirement_taken: std::sync::atomic::AtomicBool,
    }

    impl AsFd for Listener {
        fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            self.socket.as_fd()
        }
    }

    impl AsRawFd for Listener {
        fn as_raw_fd(&self) -> std::os::fd::RawFd {
            self.socket.as_raw_fd()
        }
    }
    pub type Stream = UnixStream;

    pub fn home_hash(root: &Path) -> u64 {
        let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
        crate::fnv1a(root.as_os_str().as_encoded_bytes())
    }

    pub fn address(_root: &Path, hash: u64) -> PathBuf {
        socket_dir().join(format!("{hash:016x}.sock"))
    }

    pub fn connect(path: &Path) -> io::Result<UnixStream> {
        connect_for(path, geteuid().as_raw())
    }

    pub(super) fn connect_for(path: &Path, owner: u32) -> io::Result<UnixStream> {
        validate_directory(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        let stream = UnixStream::connect(path)?;
        verify_uid(peer_uid(&stream)?, owner)?;
        Ok(stream)
    }

    pub fn remove(path: &Path) {
        if let Ok(_claim) = claim(path) {
            let _ = fs::remove_file(path);
        }
    }

    pub(super) fn claim_file(socket: &Path) -> io::Result<fs::File> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(socket.with_extension("lock"))?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != geteuid().as_raw() || meta.mode() & 0o777 != 0o600 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid endpoint claim",
            ));
        }
        Ok(file)
    }

    fn claim(socket: &Path) -> io::Result<fs::File> {
        validate_directory(socket.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        let claim = claim_file(socket)?;
        crate::fs::try_lock(&claim)?;
        Ok(claim)
    }

    pub fn bind(socket: &Path, link: &Path) -> io::Result<Listener> {
        prepare_socket_dir(socket.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        // The claim inode is never unlinked: a waiter must not lock an old
        // inode while another daemon creates and locks a replacement.
        let claim = claim(socket)?;
        match fs::remove_file(socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(socket)?;
        if fs::read_link(link).ok().as_deref() != Some(socket) {
            let _ = fs::remove_file(link);
            // Only a convenience: a home on a filesystem without symlinks
            // still works.
            let _ = std::os::unix::fs::symlink(socket, link);
        }
        Ok(Listener {
            socket: listener,
            claim,
            retirement_taken: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// A connection the daemon writes to without blocking: its send buffer
    /// holds a whole write batch, so the writer waits on the socket once a
    /// batch rather than once each default 8 KiB macOS buffers.
    pub fn accept(listener: &Listener) -> io::Result<UnixStream> {
        accept_for(listener, geteuid().as_raw())
    }

    pub(super) fn accept_for(listener: &Listener, owner: u32) -> io::Result<UnixStream> {
        let (stream, _) = listener.socket.accept()?;
        verify_uid(peer_uid(&stream)?, owner)?;
        // Only a speed-up: a socket that keeps its default still works.
        let _ = rustix::net::sockopt::set_socket_send_buffer_size(&stream, SEND_BUFFER);
        Ok(stream)
    }

    const SEND_BUFFER: usize = 256 << 10;

    pub fn listener_nonblocking(listener: &Listener, nonblocking: bool) -> io::Result<()> {
        listener.socket.set_nonblocking(nonblocking)
    }

    pub fn adopt(file: fs::File, endpoint: &Path) -> io::Result<Listener> {
        let number = std::env::var("A_ENDPOINT_CLAIM_FD")
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "missing endpoint claim"))?
            .parse::<i32>()
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        let claim = crate::InheritedNumber::from(number).duplicate()?;
        adopt_claim(file, claim, endpoint)
    }

    pub fn adopt_claim(file: fs::File, claim: fs::File, endpoint: &Path) -> io::Result<Listener> {
        validate_directory(endpoint.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        let probe = claim_file(endpoint)?;
        let actual = claim.metadata()?;
        let expected = probe.metadata()?;
        if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino())
            || !matches!(crate::fs::try_lock(&probe), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unowned endpoint claim",
            ));
        }
        crate::fs::try_lock(&claim)?;
        let fd = OwnedFd::from(file);
        if rustix::net::sockopt::socket_type(&fd)? != rustix::net::SocketType::STREAM {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let listener = UnixListener::from(fd);
        if listener.local_addr()?.as_pathname() != Some(endpoint) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "wrong listener endpoint",
            ));
        }
        // SO_ACCEPTCONN is unavailable on macOS. listen validates that this
        // bound stream can serve as a listener, and rejects connected sockets.
        rustix::net::listen(&listener, 128)?;
        Ok(Listener {
            socket: listener,
            claim,
            retirement_taken: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub struct Retirement {
        claim: fs::File,
        path: PathBuf,
    }

    pub fn retirement(listener: &Listener) -> io::Result<Retirement> {
        let claim = listener.claim.try_clone()?;
        let path = listener
            .socket
            .local_addr()?
            .as_pathname()
            .ok_or(io::ErrorKind::InvalidInput)?
            .to_owned();
        if listener
            .retirement_taken
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "endpoint retirement already taken",
            ));
        }
        Ok(Retirement { claim, path })
    }

    pub fn retire(retirement: Retirement) -> io::Result<()> {
        match fs::remove_file(retirement.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        retirement.claim.unlock()
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

    pub fn verify_peer(stream: &UnixStream) -> io::Result<()> {
        verify_uid(peer_uid(stream)?, geteuid().as_raw())
    }

    pub(super) fn verify_uid(peer: u32, owner: u32) -> io::Result<()> {
        if peer != owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "endpoint peer belongs to another user",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        Ok(rustix::net::sockopt::socket_peercred(stream)?.uid.as_raw())
    }

    #[cfg(not(target_os = "linux"))]
    fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: the live socket and both writable scalar outputs outlive the call.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }

    pub(super) fn validate_directory(dir: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir() || meta.uid() != geteuid().as_raw() || meta.mode() & 0o777 != 0o700 {
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

    fn prepare_socket_dir(dir: &Path) -> io::Result<()> {
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        validate_directory(dir)
    }

    fn socket_dir() -> PathBuf {
        let fallback = PathBuf::from(format!("/tmp/a-{}", geteuid().as_raw()));
        // Pathname sockets retain filesystem access control. A missing XDG
        // directory also works without logind, in SSH sessions and containers;
        // /tmp need not permit executable files.
        #[cfg(target_os = "linux")]
        return runtime_dir(
            std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            &PathBuf::from(format!("/run/user/{}", geteuid().as_raw())),
            fallback,
        );
        #[cfg(not(target_os = "linux"))]
        fallback
    }

    #[cfg(target_os = "linux")]
    pub(super) fn runtime_dir(xdg: Option<PathBuf>, login: &Path, fallback: PathBuf) -> PathBuf {
        if let Some(dir) = xdg
            && dir.is_absolute()
            && validate_directory(&dir).is_ok()
        {
            return dir;
        }
        if validate_directory(login).is_ok() {
            return login.to_owned();
        }
        fallback
    }
}

#[cfg(windows)]
#[path = "endpoint_windows.rs"]
pub(crate) mod imp;

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::io::{self, Write};

    use crate::Home;

    fn home(label: &str) -> Home {
        Home::from(std::env::temp_dir().join(format!("a-endpoint-{label}-{}", std::process::id())))
    }

    #[cfg(unix)]
    #[test]
    fn a_second_listener_cannot_unlink_a_live_or_missing_socket() {
        use std::os::unix::fs::MetadataExt;
        let home = home("claim");
        std::fs::create_dir_all(home.root()).unwrap();
        let listener = home.bind().unwrap();
        let endpoint = home.endpoint();
        let before = std::fs::metadata(endpoint.path()).unwrap().ino();
        assert!(home.bind().is_err());
        endpoint.remove();
        assert_eq!(std::fs::metadata(endpoint.path()).unwrap().ino(), before);
        let _client = endpoint.connect().unwrap();
        drop(listener.accept().unwrap());
        std::fs::remove_file(endpoint.path()).unwrap();
        assert!(
            home.bind().is_err(),
            "a missing socket does not release its claim"
        );
        drop(listener);
        drop(home.bind().unwrap());
        endpoint.remove();
        std::fs::remove_dir_all(home.root()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_inherited_listener_requires_its_original_exclusive_claim() {
        use std::os::fd::AsFd;
        let home = home("adopt");
        std::fs::create_dir_all(home.root()).unwrap();
        let listener = home.bind().unwrap();
        let duplicate = || std::fs::File::from(listener.as_fd().try_clone_to_owned().unwrap());
        let endpoint = home.endpoint();
        let unowned = super::imp::claim_file(endpoint.path()).unwrap();
        assert!(super::imp::adopt_claim(duplicate(), unowned, endpoint.path()).is_err());
        let adopted = super::imp::adopt_claim(
            duplicate(),
            listener.0.claim.try_clone().unwrap(),
            endpoint.path(),
        )
        .unwrap();
        drop(listener);
        assert!(home.bind().is_err());
        let _client = endpoint.connect().unwrap();
        drop(super::imp::accept(&adopted).unwrap());
        drop(adopted);
        endpoint.remove();
        std::fs::remove_dir_all(home.root()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn foreign_user_identity_is_refused_before_protocol_io() {
        use std::io::Read;
        let home = home("foreign");
        std::fs::create_dir_all(home.root()).unwrap();
        let listener = home.bind().unwrap();
        let endpoint = home.endpoint();
        // Expecting a different UID simulates a foreign peer while still
        // exercising native credentials and the complete accept/connect paths.
        let foreign = rustix::process::geteuid().as_raw().wrapping_add(1);
        let mut client = endpoint.connect().unwrap();
        assert_eq!(
            super::imp::accept_for(&listener.0, foreign)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(client.read(&mut [0]).unwrap(), 0);
        assert_eq!(
            super::imp::connect_for(endpoint.path(), foreign)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let mut server = listener.accept().unwrap();
        assert_eq!(server.read(&mut [0]).unwrap(), 0);
        let _client = endpoint.connect().unwrap();
        drop(listener.accept().unwrap());
        drop(listener);
        endpoint.remove();
        std::fs::remove_dir_all(home.root()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn insecure_or_symlinked_runtime_directories_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let home = home("directory");
        std::fs::create_dir_all(home.root()).unwrap();
        let dir = home.root().join("runtime");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(super::imp::validate_directory(&dir).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        super::imp::validate_directory(&dir).unwrap();
        let link = home.root().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(super::imp::validate_directory(&link).is_err());
        std::fs::remove_dir_all(home.root()).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_runtime_directories_use_a_private_fallback_without_logind() {
        use std::os::unix::fs::PermissionsExt;
        let home = home("runtime");
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::set_permissions(home.root(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let missing = home.root().join("missing");
        let fallback = home.root().join("fallback");
        assert_eq!(
            super::imp::runtime_dir(None, &missing, fallback.clone()),
            fallback
        );
        assert!(!missing.exists());
        assert_eq!(
            super::imp::runtime_dir(Some(home.root().to_owned()), &missing, fallback.clone()),
            home.root()
        );
        assert_eq!(
            super::imp::runtime_dir(None, home.root(), fallback.clone()),
            home.root()
        );
        std::fs::set_permissions(home.root(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            super::imp::runtime_dir(Some(home.root().to_owned()), &missing, fallback.clone()),
            fallback
        );
        std::fs::remove_dir_all(home.root()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn only_one_retirement_can_unpublish_before_releasing_the_claim() {
        let home = home("retirement");
        std::fs::create_dir_all(home.root()).unwrap();
        let listener = home.bind().unwrap();
        let retirement = listener.retirement().unwrap();
        assert!(listener.retirement().is_err());
        assert!(home.bind().is_err());
        retirement.retire().unwrap();
        assert!(!home.endpoint().path().exists());
        let successor = home.bind().unwrap();
        drop(listener);
        let _client = home.endpoint().connect().unwrap();
        drop(successor.accept().unwrap());
        drop(successor);
        home.endpoint().remove();
        std::fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn claim_holder_child() {
        let Some(root) = std::env::var_os("A_ENDPOINT_TEST_CHILD") else {
            return;
        };
        let home = Home::from(std::path::PathBuf::from(root));
        let _listener = home.bind().unwrap();
        println!("claimed");
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn a_crash_releases_the_claim_and_recovers_the_stale_endpoint() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let home = home("crash");
        std::fs::create_dir_all(home.root()).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "endpoint::tests::claim_holder_child",
                "--nocapture",
            ])
            .env("A_HOME", home.root())
            .env("A_ENDPOINT_TEST_CHILD", home.root())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert!(
                output.read_line(&mut line).unwrap() != 0,
                "claim child exited: {}",
                child.wait().unwrap()
            );
            if line.contains("claimed") {
                break;
            }
            line.clear();
        }
        let excluded = home.bind().is_err();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(excluded);
        let listener = home.bind().unwrap();
        #[cfg(unix)]
        {
            let _client = home.endpoint().connect().unwrap();
            drop(listener.accept().unwrap());
        }
        drop(listener);
        home.endpoint().remove();
        std::fs::remove_dir_all(home.root()).unwrap();
    }

    /// A client that reads slowly costs the daemon's writer one wait per
    /// batch it writes, not one per default socket buffer (8 KiB on macOS).
    #[cfg(unix)]
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
