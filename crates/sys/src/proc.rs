//! Owned process scopes, explicit resource inheritance, and exit observation.
//!
//! [`ProcessLease`] holds live authority; [`RecordedIdentity`] is diagnostic
//! data. Windows jobs die with their daemon, leaving no recovery handles.

use std::fs::File;
use std::io;
use std::process::{Command, ExitStatus};

use crate::Signal;

pub(crate) use imp::passed;
#[cfg(unix)]
pub(crate) use imp::raw;
#[cfg(windows)]
pub(crate) use imp::spawn_acknowledged;
pub use imp::{Child, ExitWatch};

#[derive(Clone, Copy)]
pub(crate) enum SpawnEnvironment {
    Exact,
    InheritSnapshot,
}

/// Finds native Bash from a captured PATH and installed locations.
pub fn find_bash(search: Option<&str>) -> Option<String> {
    crate::shell::find_bash(search).map(|path| path.to_string_lossy().into_owned())
}

/// Persisted identity for diagnostics; it never owns native resources.
///
/// ```compile_fail
/// sys::RecordedIdentity(42).signal(sys::Signal::Kill);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordedIdentity(pub u32);

// Each observation opens at most this many native exit registrations. Once
// any watched member exits, the next observation discovers the remaining group.
const EXIT_WATCH_BATCH: usize = 64;

impl RecordedIdentity {
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

/// Live containment; dropping it terminates everything it still owns.
pub struct ProcessLease {
    id: RecordedIdentity,
    #[cfg(unix)]
    child: Child,
    #[cfg(windows)]
    handle: imp::Job,
}

impl ProcessLease {
    #[cfg(unix)]
    pub(crate) fn new(child: &Child) -> Self {
        Self {
            id: RecordedIdentity(child.id()),
            child: child.clone(),
        }
    }

    /// The job the build before handed over, its leader `id`.
    #[cfg(windows)]
    pub(crate) const fn handed(
        id: RecordedIdentity,
        handle: std::os::windows::io::OwnedHandle,
    ) -> Self {
        Self {
            id,
            handle: imp::Job::new(handle),
        }
    }

    /// The job, no longer stopped as the lease drops: for the next build.
    #[cfg(windows)]
    pub(crate) fn into_job(self) -> std::os::windows::io::OwnedHandle {
        let lease = std::mem::ManuallyDrop::new(self);
        // SAFETY: the lease is never dropped, so the handle read out of it
        // has this one owner.
        unsafe { std::ptr::read(&raw const lease.handle) }.into_handle()
    }

    pub const fn id(&self) -> RecordedIdentity {
        self.id
    }

    pub fn signal(&self, signal: Signal) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.child.signal_group(signal)
        }
        #[cfg(windows)]
        {
            let _ = signal;
            imp::terminate_group(&self.handle)
        }
    }

    pub fn members(&self) -> io::Result<Vec<u32>> {
        #[cfg(unix)]
        {
            if !self.child.grouped() {
                return Ok(if self.child.exited() {
                    Vec::new()
                } else {
                    vec![self.child.id()]
                });
            }
            let members = self.id.members();
            Ok(members
                .into_iter()
                .filter(|pid| *pid != self.child.id() || !self.child.exited())
                .collect())
        }
        #[cfg(windows)]
        {
            imp::members(&self.handle)
        }
    }

    /// Opens a bounded batch of exit notifications under this live lease.
    /// Observe again after an exit to cover groups of any size.
    pub fn exit_watches(&self) -> io::Result<Vec<ExitWatch>> {
        let mut watches = Vec::new();
        for pid in self.members()? {
            #[cfg(unix)]
            let watch = imp::member_watch(self.id.0, pid);
            #[cfg(windows)]
            let watch = imp::member_watch(&self.handle, pid);
            if let Some(watch) = watch? {
                watches.push(watch);
                if watches.len() == EXIT_WATCH_BATCH {
                    break;
                }
            }
        }
        Ok(watches)
    }

    pub fn alive(&self) -> bool {
        self.members().map_or(true, |members| !members.is_empty())
    }
}

impl Drop for ProcessLease {
    fn drop(&mut self) {
        let _ = self.signal(Signal::Kill);
    }
}

/// Containment cannot be lost between launch and asynchronous adoption.
pub struct ContainedChild {
    pub child: Child,
    pub group: ProcessLease,
    pub capture: Option<OutputCapture>,
}

/// Output that must be copied asynchronously to its output stream before completion.
pub struct OutputCapture {
    pub reader: ProcessPipe,
    pub file: File,
}

impl std::ops::Deref for ContainedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}
impl std::ops::DerefMut for ContainedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
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
    pub(crate) const fn raw(self) -> i32 {
        self.0
    }

    /// Duplicates through the OS before constructing an owner. The original
    /// number may belong to another Rust owner and is never closed here.
    /// Both resources stop being inheritable so tools cannot retain daemon locks.
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

/// What a child can be handed through [`Pass::passed`].
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

/// Creates a command in an owned native scope before any user code runs.
pub fn spawn_group(cmd: &mut Command, output: File) -> io::Result<ContainedChild> {
    #[cfg(unix)]
    {
        let child = imp::spawn_group(cmd, &output)?;
        drop(output);
        let group = ProcessLease::new(&child);
        Ok(ContainedChild {
            child,
            group,
            capture: None,
        })
    }
    #[cfg(windows)]
    imp::spawn_group(cmd, output)
}

/// Captures output where direct file writes cannot wake live followers.
/// The caller must drain `capture` before reporting process completion.
pub fn spawn_group_captured(cmd: &mut Command, output: File) -> io::Result<ContainedChild> {
    #[cfg(unix)]
    {
        let (reader, writer) = io::pipe()?;
        let input = File::open("/dev/null")?;
        let writer = File::from(std::os::fd::OwnedFd::from(writer));
        let child = unix::SpawnPlan::new(cmd, [&input, &writer, &writer]).spawn()?;
        let group = ProcessLease::new(&child);
        Ok(ContainedChild {
            child,
            group,
            capture: Some(OutputCapture {
                reader: File::from(std::os::fd::OwnedFd::from(reader)),
                file: output,
            }),
        })
    }
    #[cfg(windows)]
    imp::spawn_group_captured(cmd, output)
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

/// When the OS says process `pid` started, in its own units: equal for one
/// process, different for a later one reusing the pid. `None` when there is
/// no such process, or no way to tell.
pub fn process_started(pid: u32) -> Option<u64> {
    imp::process_started(pid)
}

/// A verified native process handle retained across daemon stop attempts.
/// Platforms unable to retain nonchild identity refuse forced termination.
pub struct RecordedProcess {
    #[cfg(windows)]
    process: imp::RecordedProcess,
    #[cfg(target_os = "linux")]
    process: std::os::fd::OwnedFd,
}

impl RecordedProcess {
    pub fn open(record: &crate::crash::Record) -> io::Result<Self> {
        #[cfg(windows)]
        return imp::RecordedProcess::open(record.pid, record.token)
            .map(|process| Self { process });
        #[cfg(target_os = "linux")]
        {
            let process = imp::exit_watch(record.pid).map_err(|error| {
                if error.raw_os_error() == Some(libc::ENOSYS) {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "forced stop requires pidfd support; use daemon RPC or its supervisor",
                    )
                } else {
                    error
                }
            })?;
            if record.token.is_none() || record.token != process_started(record.pid) {
                return Err(io::Error::other(
                    "process no longer matches the daemon record",
                ));
            }
            Ok(Self { process })
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = record;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "forced stop requires a retained process identity; use daemon RPC or its supervisor",
            ))
        }
    }
    pub fn signal(&self, signal: Signal) -> io::Result<()> {
        #[cfg(windows)]
        return self.process.signal(signal);
        #[cfg(target_os = "linux")]
        return match rustix::process::pidfd_send_signal(&self.process, raw(signal)) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        };
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = signal;
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}

/// The name of the signal that ended a process (`SIGKILL`), if one did.
pub fn exit_signal(status: ExitStatus) -> Option<String> {
    imp::exit_signal(status)
}

/// Identifies the current boot for recorded process diagnostics. A failed
/// lookup is retried; a successful identity is stable for the process lifetime.
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
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::{Command, ExitStatus};

    use rustix::process;

    use super::Passed;
    use crate::Signal;

    pub type ExitWatch = OwnedFd;
    pub use super::unix::Child;

    pub fn spawn_daemon(
        cmd: &mut Command,
        home: &crate::Home,
        lock: fs::File,
    ) -> io::Result<Child> {
        use super::Pass;
        let listener = home.bind()?;
        let handles = [listener.passed(), lock.passed(), listener.claim_passed()];
        cmd.env("A_ENDPOINT_CLAIM_FD", "5");
        cmd.args(["--listen-fd", "3", "--lock-fd", "4"]);
        let null = fs::File::options()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        let child = super::unix::SpawnPlan::new(cmd, [&null, &null, &null])
            .inherited(&handles)
            .uncontained()
            .spawn();
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

    pub fn spawn_group(cmd: &mut Command, output: &fs::File) -> io::Result<Child> {
        let input = fs::File::open("/dev/null")?;
        super::unix::SpawnPlan::new(cmd, [&input, output, output]).spawn()
    }

    pub fn passed(fd: &impl AsRawFd) -> Passed {
        Passed(fd.as_raw_fd())
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
        let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
        // Field 22, counting from the pid; the command name before it, in
        // parentheses, may hold spaces.
        let rest = &stat[stat.iter().rposition(|b| *b == b')')? + 1..];
        std::str::from_utf8(
            rest.split(u8::is_ascii_whitespace)
                .filter(|v| !v.is_empty())
                .nth(19)?,
        )
        .ok()?
        .parse()
        .ok()
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

    pub fn member_watch(group: u32, member: u32) -> io::Result<Option<ExitWatch>> {
        let watch = match exit_watch(member) {
            Ok(watch) => watch,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        match process::getpgid(Some(pid(member)?)) {
            Ok(actual) if actual.as_raw_nonzero().get().cast_unsigned() == group => Ok(Some(watch)),
            Ok(_) | Err(rustix::io::Errno::SRCH) => Ok(None),
            Err(error) => Err(error.into()),
        }
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
            let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
            // After the command name, in parentheses: state, ppid, pgrp.
            let rest = &stat[stat.iter().rposition(|b| *b == b')')? + 1..];
            let mut fields = rest
                .split(u8::is_ascii_whitespace)
                .filter(|v| !v.is_empty());
            let state = fields.next()?;
            Some(state != b"Z" && std::str::from_utf8(fields.nth(1)?).ok()?.parse() == Ok(pgid))
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
        let pgid = pid(pgid)?;
        match process::kill_process_group(pgid, raw(signal)) {
            Err(rustix::io::Errno::PERM) if !has_members(pgid) => Ok(()),
            result => gone_is_ok(result),
        }
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
    pub(crate) fn has_members(pgid: process::Pid) -> bool {
        !matches!(
            process::test_kill_process_group(pgid),
            Err(rustix::io::Errno::SRCH)
        )
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
            Signal::Suspend => process::Signal::TSTP,
            Signal::Continue => process::Signal::CONT,
            Signal::BackgroundRead => process::Signal::TTIN,
            Signal::BackgroundWrite => process::Signal::TTOU,
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
    #[cfg(target_os = "linux")]
    #[cfg_attr(miri, ignore)]
    fn procfs_identity_and_membership_ignore_non_utf8_command_names() {
        use std::io::{BufRead, Write};
        const CHILD: &str = "A_TEST_PROC_NAME";
        if std::env::var_os(CHILD).is_some() {
            std::fs::write("/proc/self/comm", b"a ) \xff").unwrap();
            println!("ready");
            io::stdout().flush().unwrap();
            io::stdin().read_line(&mut String::new()).unwrap();
            return;
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "proc::tests::procfs_identity_and_membership_ignore_non_utf8_command_names",
                "--nocapture",
            ])
            .env(CHILD, "1");
        let (mut child, mut input, output) = spawn_stdio(&mut command).unwrap();
        let mut output = io::BufReader::new(output);
        let mut line = String::new();
        loop {
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "child never started"
            );
            if line.trim() == "ready" {
                break;
            }
            line.clear();
        }
        assert!(
            String::from_utf8(std::fs::read(format!("/proc/{}/stat", child.id())).unwrap())
                .is_err()
        );
        assert!(process_started(child.id()).is_some());
        assert!(child.group.members().unwrap().contains(&child.id()));
        input.write_all(b"release\n").unwrap();
        assert!(child.wait().unwrap().success());
    }

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
    #[cfg_attr(miri, ignore)]
    fn only_a_live_lease_can_kill_its_group() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & sleep 30"]);
        let mut child = spawn_group(
            &mut command,
            File::options().write(true).open("/dev/null").unwrap(),
        )
        .unwrap();
        child.group.signal(Signal::Kill).unwrap();
        assert!(!child.wait().unwrap().success());
        child.group.signal(Signal::Kill).unwrap();
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
    #[cfg_attr(miri, ignore)]
    fn exit_watch_batches_never_limit_group_membership() {
        use std::io::BufRead;
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!(
                "i=0; while [ \"$i\" -lt {} ]; do sleep 30 & i=$((i+1)); done; echo ready; read _",
                EXIT_WATCH_BATCH + 1,
            ),
        ]);
        let (mut child, input, output) = spawn_stdio(&mut cmd).unwrap();
        let mut ready = String::new();
        io::BufReader::new(output).read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "ready");
        let members = child.group.members().unwrap();
        let watches = child.group.exit_watches().unwrap();
        assert!(members.len() > EXIT_WATCH_BATCH);
        assert_eq!(watches.len(), EXIT_WATCH_BATCH);
        assert!(child.group.alive());
        child.group.signal(Signal::Kill).unwrap();
        child.wait().unwrap();
        drop(input);
    }

    #[test]
    fn an_exit_watch_turns_readable_when_its_process_exits() {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let readable = |fd: &std::os::fd::OwnedFd, ms: u64| {
            let mut fds = [PollFd::new(fd, PollFlags::IN)];
            let timeout = Timespec::try_from(std::time::Duration::from_millis(ms)).unwrap();
            poll(&mut fds, Some(&timeout)).unwrap() == 1
        };
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & read _"])
            .stdin(std::process::Stdio::piped());
        let (mut child, input, _output) = spawn_stdio(&mut cmd).unwrap();
        let watch = child.child.exit_watch().unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(
            std::fs::read_link(format!("/proc/self/fd/{}", watch.as_raw_fd())).unwrap(),
            std::path::Path::new("anon_inode:[pidfd]")
        );
        assert!(!readable(&watch, 50));
        // The shell and its background sleep.
        let group = RecordedIdentity(child.id());
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
        drop(input);
        assert!(readable(&watch, 5000));
        child.wait().unwrap();
        assert!(child.child.exited());
        assert!(group.alive());
        child.group.signal(Signal::Kill).unwrap();
        for _ in 0..100 {
            if child.group.members().unwrap().is_empty() {
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
        let mut child = spawn_group(
            &mut cmd,
            File::options().write(true).open("/dev/null").unwrap(),
        )
        .unwrap();
        let group = RecordedIdentity(child.id());
        assert!(group.alive());
        let pgid = rustix::process::Pid::from_raw(child.id().cast_signed()).unwrap();
        assert!(imp::has_members(pgid));
        child.group.signal(Signal::Terminate).unwrap();
        let status = child.wait().unwrap();
        assert_eq!(exit_signal(status).as_deref(), Some("SIGTERM"));
        assert!(!child.group.alive());
        drop(child);
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
pub fn spawn_stdio(cmd: &mut Command) -> io::Result<(ContainedChild, ProcessPipe, ProcessPipe)> {
    let (child, input, output, _) = spawn_stdio_role(cmd, Role::Contained)?;
    Ok((child, input, output))
}

/// Whether ssh may ask the person at this process's terminal: for a
/// password, a passphrase, or a host key to confirm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// It shares the caller's terminal and stderr.
    Terminal,
    /// It has no terminal to ask at; what it reports comes back on a pipe.
    Never,
}

/// SSH with the caller's environment, and its terminal as `prompt` says;
/// the last pipe is its stderr, when that is not the caller's.
pub fn spawn_ssh(
    cmd: &mut Command,
    prompt: Prompt,
) -> io::Result<(
    ContainedChild,
    ProcessPipe,
    ProcessPipe,
    Option<ProcessPipe>,
)> {
    spawn_stdio_role(cmd, Role::Ssh(prompt))
}

#[derive(Clone, Copy)]
enum Role {
    Contained,
    Ssh(Prompt),
}

fn spawn_stdio_role(
    cmd: &mut Command,
    role: Role,
) -> io::Result<(
    ContainedChild,
    ProcessPipe,
    ProcessPipe,
    Option<ProcessPipe>,
)> {
    #[cfg(unix)]
    {
        use std::os::fd::OwnedFd;
        let (child_input, input) = io::pipe()?;
        let (output, child_output) = io::pipe()?;
        let child_input = File::from(OwnedFd::from(child_input));
        let child_output = File::from(OwnedFd::from(child_output));
        let (error, child_error) = match role {
            Role::Contained => (None, File::options().write(true).open("/dev/null")?),
            Role::Ssh(Prompt::Terminal) => (
                None,
                File::from(std::os::fd::AsFd::as_fd(&io::stderr()).try_clone_to_owned()?),
            ),
            Role::Ssh(Prompt::Never) => {
                let (error, child_error) = io::pipe()?;
                (
                    Some(File::from(OwnedFd::from(error))),
                    File::from(OwnedFd::from(child_error)),
                )
            }
        };
        let plan = unix::SpawnPlan::new(cmd, [&child_input, &child_output, &child_error]);
        let plan = match role {
            Role::Contained => plan,
            Role::Ssh(Prompt::Terminal) => plan.terminal().inherit_snapshot(),
            // Its own session: no controlling terminal to open for a prompt.
            Role::Ssh(Prompt::Never) => plan.inherit_snapshot(),
        };
        let child = plan.spawn()?;
        let input = File::from(OwnedFd::from(input));
        let output = File::from(OwnedFd::from(output));
        let group = ProcessLease::new(&child);
        Ok((
            ContainedChild {
                child,
                group,
                capture: None,
            },
            input,
            output,
            error,
        ))
    }
    #[cfg(windows)]
    imp::spawn_stdio_role(
        cmd,
        match role {
            Role::Contained => None,
            Role::Ssh(prompt) => Some(prompt),
        },
    )
}

#[cfg(unix)]
#[path = "proc_unix.rs"]
pub(crate) mod unix;
