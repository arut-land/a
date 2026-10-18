//! Platform specifics. The only crate allowed to use `unsafe`.
#![allow(unsafe_code)]

pub mod config;
#[cfg(unix)]
pub mod install;
#[cfg(unix)]
mod login;
#[cfg(unix)]
mod runtime;
#[cfg(unix)]
pub mod tty;

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

pub use config::Config;
#[cfg(unix)]
pub use login::login_env;
#[cfg(unix)]
pub use runtime::{Runtime, Staged, VERSION};

/// Locations under the per-user home (`$A_HOME`, default `~/.a`).
#[derive(Clone, Debug)]
pub struct Home {
    root: PathBuf,
}

impl From<PathBuf> for Home {
    fn from(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Home {
    pub fn from_env() -> io::Result<Self> {
        let root = match std::env::var_os("A_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => std::env::home_dir()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?
                .join(".a"),
        };
        Ok(root.into())
    }

    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn spawn_lock(&self) -> PathBuf {
        self.run_dir().join("daemon.lock")
    }

    /// Locked by the running daemon for its whole life, and holds its pid.
    pub fn daemon_pid_file(&self) -> PathBuf {
        self.run_dir().join("daemon.pid")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Where tools keep output too large for a session's log, a private
    /// directory per session.
    pub fn spill_dir(&self) -> PathBuf {
        self.root.join("spill")
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.root.join("logs").join("daemon.log")
    }

    /// The user's settings; see [`Config`].
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    #[cfg(unix)]
    pub fn runtime(&self) -> Runtime {
        Runtime::new(self.root.join("runtime"))
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

/// How [`signal_process`] asks a process to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM: exit cleanly.
    Terminate,
    /// SIGKILL.
    Kill,
}

/// Stable across builds, unlike `std`'s hashers, so every version of `a`
/// finds the same socket and the same staging stamps.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(unix)]
pub use unix::*;

#[cfg(not(unix))]
pub fn boot_id() -> Option<&'static str> {
    None
}

#[cfg(not(unix))]
pub fn kill_process_group(_pgid: u32) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(not(unix))]
pub fn signal_group(_pgid: u32, _signal: Signal) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(not(unix))]
pub fn group_alive(_pgid: u32) -> bool {
    false
}

#[cfg(not(unix))]
pub fn new_session(_cmd: &mut std::process::Command) {}

#[cfg(not(unix))]
pub fn signal_name(raw: i32) -> String {
    format!("signal {raw}")
}

#[cfg(not(unix))]
pub fn signal_process(_pid: u32, _signal: Signal) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, RawFd};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
    use rustix::process::{self, getuid};

    use super::fnv1a;

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

        /// Binds the daemon socket, replacing a stale one.
        pub fn bind(&self) -> io::Result<UnixListener> {
            prepare_socket_dir()?;
            let socket = self.socket();
            let _ = fs::remove_file(&socket);
            UnixListener::bind(socket)
        }
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

    /// Identifies the current boot, so recorded pgids are never acted on after
    /// a reboot. Computed once per process.
    pub fn boot_id() -> Option<&'static str> {
        static BOOT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        BOOT.get_or_init(read_boot_id).as_deref()
    }

    #[cfg(target_os = "linux")]
    fn read_boot_id() -> Option<String> {
        let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(id.trim().to_owned())
    }

    #[cfg(not(target_os = "linux"))]
    fn read_boot_id() -> Option<String> {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
            .ok()?;
        let id = String::from_utf8(out.stdout).ok()?;
        Some(id.trim().to_owned()).filter(|id| !id.is_empty())
    }

    /// Sends SIGKILL to a whole process group.
    pub fn kill_process_group(pgid: u32) -> io::Result<()> {
        signal_group(pgid, super::Signal::Kill)
    }

    /// Sends `signal` to a whole process group.
    pub fn signal_group(pgid: u32, signal: super::Signal) -> io::Result<()> {
        gone_is_ok(process::kill_process_group(pid(pgid)?, raw(signal)))
    }

    /// Whether any process of group `pgid` is still alive.
    pub fn group_alive(pgid: u32) -> bool {
        pid(pgid).is_ok_and(|pgid| {
            matches!(
                process::test_kill_process_group(pgid),
                Ok(()) | Err(rustix::io::Errno::PERM)
            )
        })
    }

    /// Sends `signal` to a process.
    pub fn signal_process(pid: u32, signal: super::Signal) -> io::Result<()> {
        gone_is_ok(process::kill_process(self::pid(pid)?, raw(signal)))
    }

    fn raw(signal: super::Signal) -> process::Signal {
        match signal {
            super::Signal::Terminate => process::Signal::TERM,
            super::Signal::Kill => process::Signal::KILL,
        }
    }

    /// Starts `cmd`'s process in a session of its own: no controlling
    /// terminal, so a prompt for a password fails instead of waiting, and a
    /// process group (its pgid is its pid) that can be signalled whole.
    pub fn new_session(cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs in the forked child before exec and only
        // calls setsid(2), which is async-signal-safe and allocates nothing.
        unsafe {
            cmd.pre_exec(|| {
                process::setsid()?;
                Ok(())
            });
        }
    }

    /// A signal's name, such as `SIGKILL`.
    pub fn signal_name(raw: i32) -> String {
        use process::Signal as S;
        let name = match S::from_named_raw(raw) {
            Some(S::HUP) => "SIGHUP",
            Some(S::INT) => "SIGINT",
            Some(S::QUIT) => "SIGQUIT",
            Some(S::ILL) => "SIGILL",
            Some(S::TRAP) => "SIGTRAP",
            Some(S::ABORT) => "SIGABRT",
            Some(S::BUS) => "SIGBUS",
            Some(S::FPE) => "SIGFPE",
            Some(S::KILL) => "SIGKILL",
            Some(S::USR1) => "SIGUSR1",
            Some(S::SEGV) => "SIGSEGV",
            Some(S::USR2) => "SIGUSR2",
            Some(S::PIPE) => "SIGPIPE",
            Some(S::ALARM) => "SIGALRM",
            Some(S::TERM) => "SIGTERM",
            _ => return format!("signal {raw}"),
        };
        name.to_owned()
    }

    fn pid(raw: u32) -> io::Result<process::Pid> {
        i32::try_from(raw)
            .ok()
            .and_then(process::Pid::from_raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"))
    }

    /// A process that is already gone has nothing left to signal.
    fn gone_is_ok(result: rustix::io::Result<()>) -> io::Result<()> {
        match result {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Leaves the spawning terminal's session so its hangup never reaches us.
    pub fn detach_session() -> io::Result<()> {
        process::setsid()?;
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

/// Makes what was written to `file` durable, in write order. On Apple this
/// is a write barrier (`F_BARRIERFSYNC`, ~0.25 ms) rather than the cache
/// flush `sync_data` does there (`F_FULLFSYNC`, ~2.6 ms): writes still land
/// in order, but the newest may be lost on power loss. A filesystem without
/// barriers gets the full sync.
pub fn sync_data(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `fcntl` on a descriptor `file` owns and keeps open; the
        // command takes no argument.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) } != -1 {
            return Ok(());
        }
    }
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn kills_a_whole_process_group() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        kill_process_group(child.id()).unwrap();
        assert!(!child.wait().unwrap().success());
        kill_process_group(child.id()).unwrap();
        assert!(boot_id().is_some_and(|id| !id.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn a_new_session_leads_its_own_group_and_names_signals() {
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        new_session(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        assert!(group_alive(child.id()));
        signal_group(child.id(), Signal::Terminate).unwrap();
        child.wait().unwrap();
        assert!(!group_alive(child.id()));
        assert_eq!(signal_name(9), "SIGKILL");
        assert_eq!(signal_name(15), "SIGTERM");
        assert_eq!(signal_name(6), "SIGABRT");
    }

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
