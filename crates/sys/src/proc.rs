//! Processes: groups a command's whole tree lives in, handles a child is
//! given, and watches on a process's exit.
//!
//! A [`Group`] is a Unix process group (its pgid is its leader's pid) or,
//! on Windows, a job object named after its leader, so either is known by
//! a number that a log can record and a later daemon act on.

use std::fs::File;
use std::io;
use std::process::{Command, ExitStatus};

use crate::Signal;

pub use imp::ExitWatch;
#[cfg(unix)]
pub(crate) use imp::keep;
pub(crate) use imp::passed;
#[cfg(unix)]
pub(crate) use imp::raw;

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

impl From<i32> for Passed {
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

/// Lets the child `cmd` starts inherit `handles`, and only it: they stay
/// uninheritable in this process, so no child another thread starts
/// meanwhile holds them.
pub fn pass<const N: usize>(cmd: &mut Command, handles: [Passed; N]) -> io::Result<()> {
    imp::pass(cmd, handles)
}

/// Adopts the locked daemon record our parent passed down: the lock is the
/// open file's, so it is ours now, and was never free.
pub fn adopt_lock(passed: Passed) -> io::Result<File> {
    imp::adopt_lock(passed)
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
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::process::{Command, ExitStatus};

    use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
    use rustix::process;

    use super::Passed;
    use crate::Signal;

    pub type ExitWatch = OwnedFd;

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

    /// Marks an adopted descriptor close-on-exec again, as our own are.
    pub fn keep(fd: BorrowedFd<'_>) -> io::Result<()> {
        fcntl_setfd(fd, fcntl_getfd(fd)? | FdFlags::CLOEXEC)?;
        Ok(())
    }

    pub fn adopt_lock(Passed(fd): Passed) -> io::Result<fs::File> {
        // SAFETY: the parent (`a` itself) passes the fd number of the file
        // it opened and locked for us; nothing else in this process owns
        // that fd. `metadata` below rejects anything that is not a file.
        let file = unsafe { fs::File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the lock fd is not a file",
            ));
        }
        keep(file.as_fd())?;
        Ok(file)
    }

    pub fn detach_session() -> io::Result<()> {
        process::setsid()?;
        Ok(())
    }

    pub fn lead(cmd: &mut Command) {
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

    /// A signal's name, such as `SIGKILL`.
    pub(crate) fn signal_name(raw: i32) -> String {
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
}

/// Windows until its daemon lands (job objects, handle lists, process
/// handles as exit watches): commands still lead a console process group
/// of their own, and what needs the rest says it is unsupported.
#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::{AsRawHandle, OwnedHandle};
    use std::process::{Command, ExitStatus};

    use super::Passed;
    use crate::Signal;

    /// A process handle, signalled when it exits.
    pub type ExitWatch = OwnedHandle;

    /// `CREATE_NEW_PROCESS_GROUP`.
    const NEW_GROUP: u32 = 0x0000_0200;

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "not supported on Windows yet")
    }

    pub fn passed(handle: &impl AsRawHandle) -> Passed {
        Passed(i32::try_from(handle.as_raw_handle() as isize).unwrap_or(-1))
    }

    pub fn pass<const N: usize>(_cmd: &mut Command, _handles: [Passed; N]) -> io::Result<()> {
        Err(unsupported())
    }

    pub fn adopt_lock(_passed: Passed) -> io::Result<File> {
        Err(unsupported())
    }

    /// A detached daemon is started detached; there is no session to leave.
    #[allow(clippy::unnecessary_wraps, reason = "Unix can fail")]
    pub fn detach_session() -> io::Result<()> {
        Ok(())
    }

    pub fn lead(cmd: &mut Command) {
        std::os::windows::process::CommandExt::creation_flags(cmd, NEW_GROUP);
    }

    /// When the machine booted, in Unix seconds rounded to the minute: the
    /// clock less the time since boot (`GetTickCount64`), so the same for
    /// every process of one boot. Rounding absorbs the two clocks' drift; a
    /// wall-clock jump can make one boot look like another, which only
    /// leaves a recorded group unkilled.
    pub fn read_boot_id() -> Option<String> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetTickCount64() -> u64;
        }
        // SAFETY: takes nothing and cannot fail.
        let since_boot = unsafe { GetTickCount64() } / 1000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let booted = (now.checked_sub(since_boot)? + 30) / 60 * 60;
        Some(format!("boot-{booted}"))
    }

    pub fn process_started(_pid: u32) -> Option<u64> {
        None
    }

    pub fn exit_watch(_pid: u32) -> io::Result<OwnedHandle> {
        Err(unsupported())
    }

    pub fn group_members(_pgid: u32) -> Vec<u32> {
        Vec::new()
    }

    pub fn signal_group(_pgid: u32, _signal: Signal) -> io::Result<()> {
        Err(unsupported())
    }

    pub fn group_alive(_pgid: u32) -> bool {
        false
    }

    pub fn signal_process(_pid: u32, _signal: Signal) -> io::Result<()> {
        Err(unsupported())
    }

    pub fn exit_signal(_status: ExitStatus) -> Option<String> {
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
