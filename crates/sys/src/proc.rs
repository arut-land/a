//! Processes: groups a command's whole tree lives in, handles a child is
//! given, and watches on a process's exit.
//!
//! A [`Group`] is a Unix process group (its pgid is its leader's pid) or,
//! on Windows, a kill-on-close job object indexed by its leader. Windows
//! trees die with their owning daemon, so recovery has none left to reap.

use std::fs::File;
use std::io;
use std::process::{Command, ExitStatus};

use crate::Signal;

pub(crate) use imp::passed;
#[cfg(unix)]
pub(crate) use imp::raw;
pub use imp::{Child, ExitWatch};

/// macOS creates std's exec-error pipe before setting CLOEXEC. A fork
/// meanwhile can keep its writer alive until that unrelated child exits.
pub(crate) fn spawn(cmd: &mut Command) -> io::Result<std::process::Child> {
    #[cfg(target_vendor = "apple")]
    let _spawning = {
        static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SPAWNING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    cmd.spawn()
}

pub(crate) fn prepare_spawn(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the hook only resets signal state with async-signal-safe
        // libc calls; it does not allocate or acquire locks after fork.
        unsafe { cmd.pre_exec(crate::signals::reset_before_exec) };
    }
    #[cfg(windows)]
    let _ = cmd;
}

/// Finds bash using the system location first, then the session's PATH.
pub fn find_bash(search: Option<&str>) -> Option<String> {
    let system = std::path::Path::new("/bin/bash");
    if system.is_file() {
        return Some(system.display().to_string());
    }
    #[cfg(windows)]
    for (variable, suffix) in [
        ("ProgramFiles", "Git/bin/bash.exe"),
        ("LOCALAPPDATA", "Programs/Git/bin/bash.exe"),
    ] {
        if let Some(root) = std::env::var_os(variable) {
            let path = std::path::PathBuf::from(root).join(suffix);
            if path.is_file() {
                return Some(path.display().to_string());
            }
        }
    }
    let name = format!("bash{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(search?)
        .map(|dir| dir.join(&name))
        .find(|path| path.is_file())
        .map(|path| path.display().to_string())
}

/// A process group: a command and everything it started, signalled whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Group(pub u32);

impl Group {
    /// Makes the process `cmd` starts lead a group of its own, with no
    /// controlling terminal (a prompt for a password fails instead of
    /// waiting), whose id is its pid.
    pub fn lead(cmd: &mut Command) {
        imp::lead(cmd);
    }

    /// Sends `signal` to every process in the group. A group that is gone
    /// has nothing left to signal.
    pub fn signal(self, signal: Signal) -> io::Result<()> {
        imp::signal_group(self.0, signal)
    }

    /// SIGKILL to the whole group.
    pub fn kill(self) -> io::Result<()> {
        self.signal(Signal::Kill)
    }

    /// Whether any process of the group is still alive. A group we may not
    /// signal (a member changed user, as sudo does) is alive too.
    pub fn alive(self) -> bool {
        imp::group_alive(self.0)
    }

    /// The live processes of the group.
    pub fn members(self) -> Vec<u32> {
        imp::group_members(self.0)
    }
}

/// A descriptor (Unix) or handle (Windows) handed to a child, as the number
/// its command line names it by. Windows handle values fit 32 bits, as it
/// guarantees for 32-bit processes sharing them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Passed(pub(crate) i32);

/// An untrusted command-line number. It never owns the resource it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InheritedNumber(i32);

impl InheritedNumber {
    /// Duplicates through the OS before constructing an owner. The original
    /// number may belong to another Rust owner and is never closed here.
    /// Both descriptors are close-on-exec so tools cannot retain daemon locks.
    pub fn duplicate(self) -> io::Result<File> {
        imp::duplicate(self.0)
    }
}

impl From<i32> for InheritedNumber {
    fn from(raw: i32) -> Self {
        Self(raw)
    }
}

impl std::fmt::Display for Passed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What a child can be handed with [`pass`].
pub trait Pass {
    fn passed(&self) -> Passed;
}

impl Pass for File {
    fn passed(&self) -> Passed {
        passed(self)
    }
}

/// Starts a detached daemon. Unix hands over the bound listener and lock;
/// Windows re-listens under the client's start lock and acknowledges binding
/// on stdout, avoiding process-owned byte-lock and handle inheritance races.
pub fn spawn_daemon(cmd: &mut Command, home: &crate::Home, lock: File) -> io::Result<Child> {
    imp::spawn_daemon(cmd, home, lock)
}

pub fn daemon_ready() -> io::Result<()> {
    imp::daemon_ready()
}

/// Creates a command's process tree as a group before any user code runs.
pub fn spawn_group(cmd: &mut Command, output: File) -> io::Result<Child> {
    imp::spawn_group(cmd, output)
}

/// An inherited daemon record whose identity and exclusive lock were verified.
#[derive(Debug)]
pub struct DaemonLock(File);

impl DaemonLock {
    /// Revalidates the destination so a lock cannot authorize another home.
    pub fn into_file(self, home: &crate::Home) -> Result<File, AdoptLockError> {
        imp::adopt_lock(self.0, home)
    }
}

#[derive(Debug)]
pub enum AdoptLockError {
    WrongRecord,
    NotExclusive,
    Unsupported,
    Io(io::Error),
}

impl std::fmt::Display for AdoptLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongRecord => f.write_str("inherited lock is not the writable daemon record"),
            Self::NotExclusive => f.write_str("inherited daemon record is not exclusively locked"),
            Self::Unsupported => f.write_str("inherited daemon locks are unsupported"),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for AdoptLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for AdoptLockError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<AdoptLockError> for io::Error {
    fn from(error: AdoptLockError) -> Self {
        let kind = match &error {
            AdoptLockError::WrongRecord | AdoptLockError::Unsupported => {
                io::ErrorKind::InvalidInput
            }
            AdoptLockError::NotExclusive => io::ErrorKind::WouldBlock,
            AdoptLockError::Io(error) => error.kind(),
        };
        Self::new(kind, error)
    }
}

/// Consumes a duplicate only if it owns the expected record's exclusive lock.
pub fn adopt_lock(file: File, home: &crate::Home) -> Result<DaemonLock, AdoptLockError> {
    imp::adopt_lock(file, home).map(DaemonLock)
}

/// Leaves the spawning terminal's session so its hangup never reaches us.
pub fn detach_session() -> io::Result<()> {
    imp::detach_session()
}

/// A handle that turns readable once process `pid` has exited (reaped or
/// not): a pidfd on Linux, a kqueue of its own elsewhere. `NotFound` when
/// it is already gone.
pub fn exit_watch(pid: u32) -> io::Result<ExitWatch> {
    imp::exit_watch(pid)
}

/// When the OS says process `pid` started, in its own units: equal for one
/// process, different for a later one reusing the pid. `None` when there is
/// no such process, or no way to tell.
pub fn process_started(pid: u32) -> Option<u64> {
    imp::process_started(pid)
}

/// A daemon identity bound to its stop target. On Windows the same owned
/// query+terminate handle checks creation time and performs termination.
pub struct RecordedProcess {
    #[cfg(windows)]
    process: imp::RecordedProcess,
    #[cfg(unix)]
    record: crate::crash::Record,
}

impl RecordedProcess {
    pub fn open(record: &crate::crash::Record) -> io::Result<Self> {
        #[cfg(windows)]
        return imp::RecordedProcess::open(record.pid, record.token)
            .map(|process| Self { process });
        #[cfg(unix)]
        {
            if !record.is_live() {
                return Err(io::Error::other(
                    "process is no longer the daemon that recorded it",
                ));
            }
            Ok(Self {
                record: record.clone(),
            })
        }
    }

    pub fn signal(&self, signal: Signal) -> io::Result<()> {
        #[cfg(windows)]
        return self.process.signal(signal);
        #[cfg(unix)]
        {
            if !self.record.is_live() {
                return Err(io::Error::other(
                    "process is no longer the daemon that recorded it",
                ));
            }
            signal_process(self.record.pid, signal)
        }
    }
}

/// Sends `signal` to a process.
pub fn signal_process(pid: u32, signal: Signal) -> io::Result<()> {
    imp::signal_process(pid, signal)
}

/// The name of the signal that ended a process (`SIGKILL`), if one did.
pub fn exit_signal(status: ExitStatus) -> Option<String> {
    imp::exit_signal(status)
}

/// Identifies the current boot, so recorded groups are never acted on
/// after a reboot. Kept once found; a failure is asked again next time,
/// never remembered.
pub fn boot_id() -> Option<&'static str> {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(id) = BOOT.get() {
        return Some(id);
    }
    let id = imp::read_boot_id().filter(|id| !id.is_empty())?;
    Some(BOOT.get_or_init(|| id))
}

#[cfg(unix)]
mod imp {
    use std::fs;
    use std::io;
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::process::{Command, ExitStatus};

    use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
    use rustix::process;

    use super::Passed;
    use crate::Signal;

    pub type ExitWatch = OwnedFd;
    pub type Child = std::process::Child;

    pub fn spawn_daemon(
        cmd: &mut Command,
        home: &crate::Home,
        lock: fs::File,
    ) -> io::Result<std::process::Child> {
        use super::Pass;
        let listener = home.bind()?;
        let handles = [listener.passed(), lock.passed()];
        cmd.args([
            "--listen-fd",
            &handles[0].to_string(),
            "--lock-fd",
            &handles[1].to_string(),
        ]);
        super::prepare_spawn(cmd);
        pass(cmd, handles)?;
        let child = super::spawn(cmd);
        drop((listener, lock));
        child
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "Windows writes the startup acknowledgement"
    )]
    pub fn daemon_ready() -> io::Result<()> {
        Ok(())
    }

    pub fn spawn_group(cmd: &mut Command, output: fs::File) -> io::Result<Child> {
        lead(cmd);
        cmd.stdin(std::process::Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output);
        super::spawn(cmd)
    }

    pub fn passed(fd: &impl AsRawFd) -> Passed {
        Passed(fd.as_raw_fd())
    }

    #[allow(clippy::unnecessary_wraps, reason = "Windows can fail")]
    pub fn pass<const N: usize>(cmd: &mut Command, fds: [Passed; N]) -> io::Result<()> {
        use std::os::unix::process::CommandExt;
        // SAFETY: between fork and exec the hook only calls fcntl, which is
        // async-signal-safe, and allocates nothing. The fds are open in the
        // parent, so in the child too.
        unsafe {
            cmd.pre_exec(move || {
                for Passed(fd) in fds {
                    let fd = BorrowedFd::borrow_raw(fd);
                    fcntl_setfd(fd, fcntl_getfd(fd)? - FdFlags::CLOEXEC)?;
                }
                Ok(())
            });
        }
        Ok(())
    }

    pub fn duplicate(fd: i32) -> io::Result<fs::File> {
        // SAFETY: fcntl accepts arbitrary integers and validates them in the
        // kernel. No BorrowedFd or owner is created from the untrusted input.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl returned a new, exclusively owned descriptor.
        let file = unsafe { fs::File::from_raw_fd(duplicate) };
        // SAFETY: these raw fcntl calls validate the number in the kernel;
        // they change only inheritance, never assume ownership of the input.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl validates fd and only changes its descriptor flags.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }

    pub fn adopt_lock(
        file: fs::File,
        home: &crate::Home,
    ) -> Result<fs::File, super::AdoptLockError> {
        use super::AdoptLockError;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let actual = file.metadata()?;
        if !actual.is_file()
            || (rustix::fs::fcntl_getfl(&file).map_err(io::Error::from)?
                & rustix::fs::OFlags::ACCMODE)
                != rustix::fs::OFlags::RDWR
        {
            return Err(AdoptLockError::WrongRecord);
        }
        let probe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(home.daemon_pid_file())?;
        let expected = probe.metadata()?;
        if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
            return Err(AdoptLockError::WrongRecord);
        }
        // A fresh open must conflict before the inherited description can
        // verify ownership. Otherwise we would silently accept an unlocked fd.
        match crate::fs::try_lock(&probe) {
            Ok(()) => return Err(AdoptLockError::NotExclusive),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        match crate::fs::try_lock(&file) {
            Ok(()) => Ok(file),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Err(AdoptLockError::NotExclusive)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn detach_session() -> io::Result<()> {
        process::setsid()?;
        Ok(())
    }

    pub fn lead(cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        super::prepare_spawn(cmd);
        // SAFETY: the closure runs in the forked child before exec and only
        // calls setsid(2), which is async-signal-safe and allocates nothing.
        unsafe {
            cmd.pre_exec(|| {
                process::setsid()?;
                Ok(())
            });
        }
    }

    #[cfg(target_os = "linux")]
    pub fn read_boot_id() -> Option<String> {
        let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(id.trim().to_owned())
    }

    /// `kern.bootsessionuuid`, asked of the kernel directly.
    #[cfg(target_vendor = "apple")]
    pub fn read_boot_id() -> Option<String> {
        // A UUID string and its NUL, with room to spare.
        let mut buf = [0u8; 64];
        let mut len = buf.len();
        // SAFETY: the name is NUL-terminated; `buf` is writable for `len`
        // bytes, which the call updates to what it wrote; no new value.
        let failed = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buf.as_mut_ptr().cast(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        } != 0;
        if failed {
            return None;
        }
        let id = std::ffi::CStr::from_bytes_until_nul(buf.get(..len)?).ok()?;
        Some(id.to_str().ok()?.trim().to_owned())
    }

    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    pub fn read_boot_id() -> Option<String> {
        None
    }

    #[cfg(target_os = "linux")]
    pub fn process_started(pid: u32) -> Option<u64> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 22, counting from the pid; the command name before it, in
        // parentheses, may hold spaces.
        let (_, rest) = stat.rsplit_once(')')?;
        rest.split_whitespace().nth(19)?.parse().ok()
    }

    /// In microseconds since the epoch.
    #[cfg(target_vendor = "apple")]
    pub fn process_started(pid: u32) -> Option<u64> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `info` is a writable buffer of exactly `size` bytes, as
        // the flavor requires; the call writes at most that.
        let written = unsafe {
            libc::proc_pidinfo(
                i32::try_from(pid).ok()?,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        // SAFETY: the call filled the whole struct.
        let info = unsafe { info.assume_init() };
        Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    pub fn process_started(_pid: u32) -> Option<u64> {
        None
    }

    #[cfg(target_os = "linux")]
    pub fn exit_watch(pid: u32) -> io::Result<OwnedFd> {
        let pid = self::pid(pid)?;
        process::pidfd_open(pid, process::PidfdFlags::empty()).map_err(gone)
    }

    /// A kqueue of its own watching the process, as a pidfd does on Linux.
    /// A zombie counts as gone.
    #[cfg(not(target_os = "linux"))]
    pub fn exit_watch(pid: u32) -> io::Result<OwnedFd> {
        use rustix::event::kqueue::{
            Event, EventFilter, EventFlags, ProcessEvents, kevent, kqueue,
        };
        let pid = self::pid(pid)?;
        let queue = kqueue()?;
        let watch = [Event::new(
            EventFilter::Proc {
                pid,
                flags: ProcessEvents::EXIT,
            },
            EventFlags::ADD | EventFlags::ONESHOT,
            std::ptr::null_mut(),
        )];
        // SAFETY: the change names a process, not a descriptor, so no
        // event it yields can refer to a closed one.
        unsafe {
            kevent(
                &queue,
                &watch,
                &mut [] as &mut [Event],
                Some(std::time::Duration::ZERO),
            )
        }
        .map_err(gone)?;
        Ok(queue)
    }

    fn gone(errno: rustix::io::Errno) -> io::Error {
        if errno == rustix::io::Errno::SRCH {
            io::ErrorKind::NotFound.into()
        } else {
            errno.into()
        }
    }

    /// Zombies left out.
    #[cfg(target_os = "linux")]
    pub fn group_members(pgid: u32) -> Vec<u32> {
        let Ok(dir) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let member = |pid: &u32| {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // After the command name, in parentheses: state, ppid, pgrp.
            let (_, rest) = stat.rsplit_once(')')?;
            let mut fields = rest.split_whitespace();
            let state = fields.next()?;
            Some(state != "Z" && fields.nth(1)?.parse() == Ok(pgid))
        };
        dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .filter(|pid| member(pid) == Some(true))
            .collect()
    }

    /// Zombies included.
    #[cfg(target_vendor = "apple")]
    pub fn group_members(pgid: u32) -> Vec<u32> {
        let Ok(pgid) = i32::try_from(pgid) else {
            return Vec::new();
        };
        let mut pids: Vec<libc::pid_t> = vec![0; 64];
        loop {
            let Ok(size) = i32::try_from(size_of_val(pids.as_slice())) else {
                return Vec::new();
            };
            // SAFETY: `pids` is a writable buffer of `size` bytes; the call
            // fills at most that and returns how many pids it wrote (or -1).
            let written = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size) };
            let Ok(count) = usize::try_from(written) else {
                return Vec::new();
            };
            // A full buffer may have cut the list short.
            if count < pids.len() {
                pids.truncate(count);
                return pids.into_iter().filter_map(|p| p.try_into().ok()).collect();
            }
            pids.resize(pids.len() * 2, 0);
        }
    }

    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    pub fn group_members(_pgid: u32) -> Vec<u32> {
        Vec::new()
    }

    pub fn signal_group(pgid: u32, signal: Signal) -> io::Result<()> {
        gone_is_ok(process::kill_process_group(pid(pgid)?, raw(signal)))
    }

    pub fn group_alive(pgid: u32) -> bool {
        pid(pgid).is_ok_and(|pgid| match process::test_kill_process_group(pgid) {
            Ok(()) => true,
            Err(rustix::io::Errno::PERM) => has_members(pgid),
            Err(_) => false,
        })
    }

    /// Whether the OS lists any process in group `pgid`. macOS answers
    /// `EPERM`, not `ESRCH`, for a few milliseconds after a group's last
    /// process is reaped, so its denial alone does not mean a member.
    #[cfg(target_vendor = "apple")]
    pub(crate) fn has_members(pgid: process::Pid) -> bool {
        let mut member: libc::pid_t = 0;
        let Ok(size) = i32::try_from(size_of_val(&member)) else {
            return true;
        };
        // SAFETY: `member` is a writable buffer of `size` bytes; the call
        // fills at most that and returns the bytes written (or -1).
        let written = unsafe {
            libc::proc_listpgrppids(pgid.as_raw_nonzero().get(), (&raw mut member).cast(), size)
        };
        written != 0
    }

    #[cfg(not(target_vendor = "apple"))]
    pub(crate) fn has_members(_pgid: process::Pid) -> bool {
        true
    }

    pub fn signal_process(pid: u32, signal: Signal) -> io::Result<()> {
        gone_is_ok(process::kill_process(self::pid(pid)?, raw(signal)))
    }

    pub(crate) const fn raw(signal: Signal) -> process::Signal {
        match signal {
            Signal::Terminate => process::Signal::TERM,
            Signal::Kill => process::Signal::KILL,
            Signal::Interrupt => process::Signal::INT,
            Signal::Hangup => process::Signal::HUP,
            Signal::Resize => process::Signal::WINCH,
        }
    }

    pub fn exit_signal(status: ExitStatus) -> Option<String> {
        std::os::unix::process::ExitStatusExt::signal(&status).map(signal_name)
    }

    /// The signals a process is commonly ended by, each with its name.
    const SIGNAL_NAMES: [(process::Signal, &str); 15] = {
        use process::Signal as S;
        [
            (S::HUP, "SIGHUP"),
            (S::INT, "SIGINT"),
            (S::QUIT, "SIGQUIT"),
            (S::ILL, "SIGILL"),
            (S::TRAP, "SIGTRAP"),
            (S::ABORT, "SIGABRT"),
            (S::BUS, "SIGBUS"),
            (S::FPE, "SIGFPE"),
            (S::KILL, "SIGKILL"),
            (S::USR1, "SIGUSR1"),
            (S::SEGV, "SIGSEGV"),
            (S::USR2, "SIGUSR2"),
            (S::PIPE, "SIGPIPE"),
            (S::ALARM, "SIGALRM"),
            (S::TERM, "SIGTERM"),
        ]
    };

    /// A signal's name, such as `SIGKILL`; one without a name is given by
    /// its number.
    pub(crate) fn signal_name(raw: i32) -> String {
        let named = process::Signal::from_named_raw(raw)
            .and_then(|signal| SIGNAL_NAMES.iter().find(|(s, _)| *s == signal));
        match named {
            Some((_, name)) => (*name).to_owned(),
            None => format!("signal {raw}"),
        }
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
}

#[cfg(windows)]
#[path = "proc_windows.rs"]
mod imp;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn adopted_daemon_lock_excludes_an_independent_opener() {
        let dir = std::env::temp_dir().join(format!("a-c02-review-lock-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("run")).unwrap();
        let home = crate::Home::from(dir.clone());
        let open = || {
            File::options()
                .read(true)
                .write(true)
                .open(home.daemon_pid_file())
                .unwrap()
        };
        let original = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(home.daemon_pid_file())
            .unwrap();
        assert!(matches!(
            adopt_lock(original.try_clone().unwrap(), &home),
            Err(AdoptLockError::NotExclusive)
        ));
        crate::fs::try_lock(&original).unwrap();
        let adopted = adopt_lock(
            InheritedNumber::from(original.as_raw_fd())
                .duplicate()
                .unwrap(),
            &home,
        )
        .unwrap();
        let contender = open();
        assert_eq!(
            crate::fs::try_lock(&contender).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(matches!(
            adopt_lock(contender, &home),
            Err(AdoptLockError::NotExclusive)
        ));
        let foreign = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join("foreign"))
            .unwrap();
        crate::fs::try_lock(&foreign).unwrap();
        assert!(matches!(
            adopt_lock(foreign, &home),
            Err(AdoptLockError::WrongRecord)
        ));
        let other_home = crate::Home::from(dir.join("other"));
        std::fs::create_dir_all(other_home.run_dir()).unwrap();
        File::create(other_home.daemon_pid_file()).unwrap();
        assert!(matches!(
            adopted.into_file(&other_home),
            Err(AdoptLockError::WrongRecord)
        ));
        assert_eq!(
            crate::fs::try_lock(&open()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(original);
        crate::fs::try_lock(&open()).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn kills_a_whole_process_group() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        Group(child.id()).kill().unwrap();
        assert!(!child.wait().unwrap().success());
        Group(child.id()).kill().unwrap();
    }

    /// Found without a subprocess, the same each time, and the one the
    /// system itself reports.
    #[test]
    fn the_boot_id_is_the_kernels_and_stable() {
        let id = imp::read_boot_id().expect("a boot id");
        assert_eq!(boot_id(), Some(id.as_str()));
        assert_eq!(boot_id(), Some(id.as_str()));
        #[cfg(target_vendor = "apple")]
        {
            let out = Command::new("sysctl")
                .args(["-n", "kern.bootsessionuuid"])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), id);
        }
    }

    #[test]
    fn an_exit_watch_turns_readable_when_its_process_exits() {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let readable = |fd: &std::os::fd::OwnedFd, ms: i64| {
            let mut fds = [PollFd::new(fd, PollFlags::IN)];
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: ms * 1_000_000,
            };
            poll(&mut fds, Some(&timeout)).unwrap() == 1
        };
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & read _"])
            .stdin(std::process::Stdio::piped());
        Group::lead(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let watch = exit_watch(child.id()).unwrap();
        assert!(!readable(&watch, 50));
        // The shell and its background sleep.
        let group = Group(child.id());
        let mut members = Vec::new();
        for _ in 0..100 {
            members = group.members();
            if members.len() == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(members.len(), 2, "{members:?}");
        assert!(members.contains(&group.0));
        drop(child.stdin.take());
        assert!(readable(&watch, 5000));
        child.wait().unwrap();
        assert_eq!(
            exit_watch(group.0).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(group.alive());
        group.kill().unwrap();
        for _ in 0..100 {
            if group.members().is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the group outlived its kill");
    }

    #[test]
    fn a_group_leader_leads_its_own_group_and_names_signals() {
        use std::os::unix::process::ExitStatusExt;
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        Group::lead(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let group = Group(child.id());
        assert!(group.alive());
        let pgid = rustix::process::Pid::from_raw(child.id().cast_signed()).unwrap();
        assert!(imp::has_members(pgid));
        group.signal(Signal::Terminate).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(exit_signal(status).as_deref(), Some("SIGTERM"));
        assert!(!group.alive());
        assert!(!imp::has_members(pgid));
        assert_eq!(
            exit_signal(ExitStatus::from_raw(9)).as_deref(),
            Some("SIGKILL")
        );
        assert_eq!(imp::signal_name(6), "SIGABRT");
        assert_eq!(exit_signal(ExitStatus::from_raw(0)), None);
    }
}

/// Parent ends of child stdio: readiness pipes on Unix, overlapped pipes on Windows.
#[cfg(unix)]
pub type ProcessPipe = File;
#[cfg(windows)]
pub type ProcessPipe = crate::Stream;

/// Starts a contained child with only its explicit stdio inherited.
pub fn spawn_stdio(cmd: &mut Command) -> io::Result<(Child, ProcessPipe, ProcessPipe)> {
    #[cfg(unix)]
    {
        use std::os::fd::OwnedFd;
        use std::process::Stdio;
        imp::lead(cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = spawn(cmd)?;
        let input = File::from(OwnedFd::from(child.stdin.take().unwrap()));
        let output = File::from(OwnedFd::from(child.stdout.take().unwrap()));
        Ok((child, input, output))
    }
    #[cfg(windows)]
    imp::spawn_stdio(cmd)
}
