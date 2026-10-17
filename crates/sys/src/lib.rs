//! Platform specifics. The only crate allowed to use `unsafe`.
#![allow(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};

/// Locations under the per-user home (`$A_HOME`, default `~/.a`).
#[derive(Clone, Debug)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    pub fn from_env() -> io::Result<Self> {
        let root = match std::env::var_os("A_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => std::env::home_dir()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?
                .join(".a"),
        };
        Ok(Self { root })
    }

    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn spawn_lock(&self) -> PathBuf {
        self.run_dir().join("daemon.lock")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.root.join("logs").join("daemon.log")
    }
}

/// The workspace a directory belongs to: the nearest ancestor holding `.git`
/// (a directory, or a file for worktrees and submodules), else the directory.
pub fn workspace_of(dir: &Path) -> PathBuf {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(&dir)
        .to_path_buf()
}

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, RawFd};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
    use rustix::process::getuid;

    /// Lets a spawned child inherit `listener`; returns its fd number.
    pub fn make_inheritable(listener: &UnixListener) -> io::Result<RawFd> {
        let fd = listener.as_fd();
        fcntl_setfd(fd, fcntl_getfd(fd)? - FdFlags::CLOEXEC)?;
        Ok(fd.as_raw_fd())
    }

    impl super::Home {
        /// The daemon socket. Kept short because `sun_path` holds ~104 bytes:
        /// `/tmp/a-<uid>/<hash of the home path>.sock`.
        pub fn socket(&self) -> PathBuf {
            let hash = fnv1a(self.root.as_os_str().as_encoded_bytes());
            socket_dir().join(format!("{hash:016x}.sock"))
        }

        /// Creates the socket directory, or verifies an existing one is a real
        /// directory owned by us with mode 0700 (it lives in shared `/tmp`).
        pub fn prepare_socket_dir(&self) -> io::Result<()> {
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
    }

    fn socket_dir() -> PathBuf {
        PathBuf::from(format!("/tmp/a-{}", getuid().as_raw()))
    }

    /// Stable across builds, unlike `std`'s hashers, so every version of `a`
    /// finds the same socket.
    fn fnv1a(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// Leaves the spawning terminal's session so its hangup never reaches us.
    pub fn detach_session() -> io::Result<()> {
        rustix::process::setsid()?;
        Ok(())
    }

    /// Adopts the listening socket our parent passed down as `fd`.
    pub fn adopt_listener(fd: RawFd) -> io::Result<UnixListener> {
        // SAFETY: the parent (`a` itself) passes the fd number of a listening
        // socket it bound and left open for us; nothing else in this process
        // owns that fd. `local_addr` below rejects anything that is not a
        // Unix socket.
        let listener = unsafe { UnixListener::from_raw_fd(fd) };
        listener.local_addr()?;
        fcntl_setfd(
            listener.as_fd(),
            fcntl_getfd(listener.as_fd())? | FdFlags::CLOEXEC,
        )?;
        Ok(listener)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_is_git_root_or_dir() {
        let tmp = std::env::temp_dir().join(format!("a-ws-{}", std::process::id()));
        let nested = tmp.join("repo/src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        let plain = workspace_of(&nested);
        std::fs::create_dir_all(tmp.join("repo/.git")).unwrap();
        let repo = workspace_of(&nested);
        std::fs::remove_dir_all(&tmp).unwrap();
        assert!(plain.ends_with("repo/src/deep"));
        assert!(repo.ends_with("repo"));
    }
}
