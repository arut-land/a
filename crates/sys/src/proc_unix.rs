//! A child remains waitable until its last live owner releases its identity.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{Passed, SpawnEnvironment};
use crate::Signal;

#[derive(Clone)]
pub struct Child(Arc<OwnedChild>);

struct OwnedChild {
    pid: u32,
    watch: Option<OwnedFd>,
    group: bool,
    /// Handed to the next build: this process's image no longer signals or
    /// reaps it, the one it execs into does.
    released: AtomicBool,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.0.pid
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.observe(false)?
            .ok_or_else(|| io::Error::other("missing child status"))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.observe(true)
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.signal(Signal::Kill)
    }

    pub(super) fn grouped(&self) -> bool {
        self.0.group
    }

    pub(super) fn exited(&self) -> bool {
        self.observe(true).is_ok_and(|s| s.is_some())
    }

    fn observe(&self, nonblocking: bool) -> io::Result<Option<ExitStatus>> {
        self.0.observe(nonblocking)
    }

    /// Gives the child up to the image this process execs into: from
    /// now on nothing here signals, waits on or reaps it. It stays this
    /// process's child, so the next image adopts it ([`Child::adopt`]).
    pub(crate) fn release(&self) -> u32 {
        self.0.released.store(true, Ordering::SeqCst);
        self.0.pid
    }

    /// The group led by `pid`, a child an earlier image of this process
    /// started and released. Only the kernel's word that it is this
    /// process's child, unreaped, makes it one: a number that names
    /// anything else is refused.
    pub(crate) fn adopt(pid: u32) -> io::Result<Self> {
        let id = libc::id_t::from(pid);
        match observe(libc::P_PID, id, true) {
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the handed process is not this process's child",
                ));
            }
            Err(error) => return Err(error),
        }
        let leader =
            rustix::process::Pid::from_raw(pid.cast_signed()).ok_or(io::ErrorKind::InvalidInput)?;
        match rustix::process::getpgid(Some(leader)) {
            Ok(group) if group == leader => {}
            // A leader already exited answers for no group, as its status
            // still pins the id.
            Err(rustix::io::Errno::SRCH) => {}
            Ok(_) | Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the handed process leads no group",
                ));
            }
        }
        let mut child = Self(Arc::new(OwnedChild {
            pid,
            watch: None,
            group: true,
            released: AtomicBool::new(false),
        }));
        match super::imp::exit_watch(pid) {
            Ok(watch) => Arc::get_mut(&mut child.0).unwrap().watch = Some(watch),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(child)
    }

    pub(super) fn signal_group(&self, signal: Signal) -> io::Result<()> {
        if self.0.released.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self.0.group {
            // The unreaped leader pins this PGID until all leases are dropped.
            if self.observe(true)?.is_some()
                && super::imp::group_members(self.id())
                    .iter()
                    .all(|pid| *pid == self.id())
            {
                return Ok(());
            }
            super::imp::signal_group(self.id(), signal)
        } else {
            self.signal(signal)
        }
    }

    fn signal(&self, signal: Signal) -> io::Result<()> {
        if self.0.released.load(Ordering::SeqCst) {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.0.watch {
            return match rustix::process::pidfd_send_signal(fd, super::raw(signal)) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
                Err(error) => Err(error.into()),
            };
        }
        // A process-global reaper outside this service revokes numeric authority.
        self.observe(true)?;
        super::imp::signal_process(self.id(), signal)
    }

    pub fn exit_watch(&self) -> io::Result<OwnedFd> {
        if let Some(watch) = &self.0.watch {
            return watch.try_clone();
        }
        // Darwin can report a child gone before kqueue registration.
        // An observed status supplies readiness without a polling worker.
        if self.observe(true)?.is_none() {
            return Err(io::ErrorKind::NotFound.into());
        }
        let (reader, writer) = io::pipe()?;
        drop(writer);
        Ok(reader.into())
    }
}

fn observe(
    kind: libc::idtype_t,
    id: libc::id_t,
    nonblocking: bool,
) -> io::Result<Option<ExitStatus>> {
    loop {
        // SAFETY: waitid initializes the writable siginfo; WNOWAIT preserves
        // exclusive child ownership until the final lease releases it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = libc::WEXITED | libc::WNOWAIT | if nonblocking { libc::WNOHANG } else { 0 };
        // SAFETY: scalar child selector and a valid output pointer.
        if unsafe { libc::waitid(kind, id, &raw mut info, flags) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        // SAFETY: successful waitid initialized siginfo's child-status union.
        if unsafe { info.si_pid() } == 0 {
            return Ok(None);
        }
        // SAFETY: as above; si_code identifies which status encoding applies.
        let status = unsafe { info.si_status() };
        let raw = if info.si_code == libc::CLD_EXITED {
            status << 8
        } else {
            status
                | if info.si_code == libc::CLD_DUMPED {
                    0x80
                } else {
                    0
                }
        };
        return Ok(Some(ExitStatus::from_raw(raw)));
    }
}

impl OwnedChild {
    fn observe(&self, nonblocking: bool) -> io::Result<Option<ExitStatus>> {
        #[cfg(target_os = "linux")]
        let target = self
            .watch
            .as_ref()
            .map(|fd| (libc::P_PIDFD, fd.as_raw_fd().cast_unsigned()));
        #[cfg(target_os = "linux")]
        let (kind, id) = target.unwrap_or((libc::P_PID, self.pid));
        #[cfg(not(target_os = "linux"))]
        let (kind, id) = (libc::P_PID, self.pid);
        #[cfg(not(target_os = "linux"))]
        return observe(kind, id, nonblocking);
        #[cfg(target_os = "linux")]
        loop {
            let result = observe(kind, id, nonblocking);
            if kind == libc::P_PIDFD {
                if result
                    .as_ref()
                    .is_err_and(|e| e.raw_os_error() == Some(libc::EINVAL))
                {
                    return observe(libc::P_PID, self.pid, nonblocking);
                }
                if result
                    .as_ref()
                    .is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock)
                {
                    if nonblocking {
                        return Ok(None);
                    }
                    wait_pidfd(self.watch.as_ref().expect("selected pidfd"))?;
                    continue;
                }
            }
            return result;
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if *self.released.get_mut() {
            return;
        }
        let owned = self.observe(true).is_ok();
        if self.group && owned {
            let _ = super::imp::signal_group(self.pid, Signal::Kill);
        }
        #[cfg(target_os = "linux")]
        if let Some(watch) = &self.watch
            && reap_pidfd(watch)
        {
            return;
        }
        if !owned {
            return;
        }
        // SAFETY: the child is still waitable and this is its sole reaper.
        while unsafe { libc::waitpid(self.pid.cast_signed(), std::ptr::null_mut(), 0) } < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn reap_pidfd(watch: &OwnedFd) -> bool {
    use rustix::io::Errno;
    use rustix::process::{WaitId, WaitIdOptions, waitid};
    use std::os::fd::AsFd;
    loop {
        match waitid(WaitId::PidFd(watch.as_fd()), WaitIdOptions::EXITED) {
            // Linux 5.3 has pidfds but cannot yet wait through one.
            Err(Errno::INVAL) => return false,
            Err(Errno::INTR) => {}
            Err(Errno::AGAIN) => {
                if wait_pidfd(watch).is_err() {
                    return true;
                }
            }
            _ => return true,
        }
    }
}

#[cfg(target_os = "linux")]
fn wait_pidfd(watch: &OwnedFd) -> io::Result<()> {
    let mut fds = [rustix::event::PollFd::new(
        watch,
        rustix::event::PollFlags::IN,
    )];
    loop {
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// The only inherited descriptors are stdio and this plan's dense allowlist.
/// All Rust allocation and validation happens before entering `posix_spawn`.
pub(crate) struct SpawnPlan<'a> {
    command: &'a Command,
    io: [&'a File; 3],
    inherited: &'a [Passed],
    group: bool,
    session: bool,
    environment: SpawnEnvironment,
}

impl<'a> SpawnPlan<'a> {
    pub(crate) fn new(command: &'a Command, io: [&'a File; 3]) -> Self {
        Self {
            command,
            io,
            inherited: &[],
            group: true,
            session: true,
            environment: SpawnEnvironment::Exact,
        }
    }
    pub(crate) fn inherited(mut self, inherited: &'a [Passed]) -> Self {
        self.inherited = inherited;
        self
    }
    pub(crate) fn terminal(mut self) -> Self {
        self.group = false;
        self.session = false;
        self
    }
    pub(crate) fn inherit_snapshot(mut self) -> Self {
        self.environment = SpawnEnvironment::InheritSnapshot;
        self
    }
    pub(crate) fn uncontained(mut self) -> Self {
        self.group = false;
        self.session = false;
        self
    }

    pub(crate) fn spawn(self) -> io::Result<Child> {
        let environment = self.environment_values();
        let directory = std::path::absolute(
            self.command
                .get_current_dir()
                .unwrap_or(std::path::Path::new(".")),
        )?;
        let program = self.command.get_program();
        // posix_spawnp searches the parent's PATH. Resolve using the child's
        // environment so one session cannot select another's executable.
        let executable = if program.as_bytes().contains(&b'/') {
            directory.join(program).into_os_string()
        } else {
            let path = environment
                .get(OsStr::new("PATH"))
                .map_or(OsStr::new("/usr/bin:/bin"), OsString::as_os_str);
            std::env::split_paths(path)
                .map(|dir| directory.join(dir).join(program))
                .find(|path| {
                    path.is_file() && rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
                })
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "executable absent from child PATH")
                })?
                .into_os_string()
        };
        let executable = string(&executable)?;
        let arguments: Vec<_> = std::iter::once(program)
            .chain(self.command.get_args())
            .map(string)
            .collect::<io::Result<_>>()?;
        let mut argv: Vec<_> = arguments.iter().map(|v| v.as_ptr().cast_mut()).collect();
        argv.push(std::ptr::null_mut());
        let env: Vec<_> = environment
            .into_iter()
            .map(|(mut key, value)| {
                if key.is_empty() || key.as_bytes().contains(&b'=') {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                key.push("=");
                key.push(value);
                string(&key)
            })
            .collect::<io::Result<_>>()?;
        let mut envp: Vec<_> = env.iter().map(|v| v.as_ptr().cast_mut()).collect();
        envp.push(std::ptr::null_mut());
        let (actions, duplicates) = self.actions(&directory)?;
        let attributes = self.attributes()?;
        #[cfg(all(target_os = "linux", not(target_env = "gnu")))]
        let pid = {
            let _ = (actions, attributes);
            linux::spawn(
                &executable,
                &argv,
                &envp,
                Some(&directory),
                &duplicates,
                self.session,
                self.group,
            )?
        };
        #[cfg(not(all(target_os = "linux", not(target_env = "gnu"))))]
        let pid = {
            let _duplicates = duplicates;
            let mut pid = 0;
            // SAFETY: every action, attribute, string and pointer array lives
            // through the call; no Rust code runs in the child before exec.
            checked(unsafe {
                libc::posix_spawn(
                    &raw mut pid,
                    executable.as_ptr(),
                    &raw const actions.0,
                    &raw const attributes.0,
                    argv.as_ptr(),
                    envp.as_ptr(),
                )
            })?;
            pid
        };
        let mut child = Child(Arc::new(OwnedChild {
            pid: pid.cast_unsigned(),
            watch: None,
            group: self.group,
            released: AtomicBool::new(false),
        }));
        match super::imp::exit_watch(child.id()) {
            Ok(watch) => Arc::get_mut(&mut child.0).unwrap().watch = Some(watch),
            // Darwin may report ESRCH for a child already exited. Its unreaped
            // status still pins identity, and an owned wait supplies readiness.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                let _ = child.kill();
                return Err(error);
            }
        }
        Ok(child)
    }
    fn environment_values(&self) -> std::collections::BTreeMap<OsString, OsString> {
        let mut environment = if matches!(self.environment, SpawnEnvironment::InheritSnapshot) {
            std::env::vars_os().collect()
        } else {
            std::collections::BTreeMap::<OsString, OsString>::new()
        };
        for (key, value) in self.command.get_envs() {
            if let Some(value) = value {
                environment.insert(key.into(), value.into());
            } else {
                environment.remove(key);
            }
        }
        environment
    }

    fn actions(&self, directory: &std::path::Path) -> io::Result<(Actions, Vec<OwnedFd>)> {
        let mut actions = Actions::new()?;
        // Keep sources out of destination slots even when stdin/out are closed
        // in the parent; dup actions must not clobber another action's source.
        let minimum =
            i32::try_from(3 + self.inherited.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
        let sources = self
            .io
            .iter()
            .map(|fd| fd.as_raw_fd())
            .chain(self.inherited.iter().map(|p| p.0));
        let mut duplicates = Vec::new();
        for (destination, source) in sources.enumerate() {
            // SAFETY: fcntl validates the borrowed descriptor, returning a new owner.
            let fd = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, minimum) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fcntl returned an exclusively owned descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // SAFETY: initialized actions, valid owned source, scalar destination.
            checked(unsafe {
                libc::posix_spawn_file_actions_adddup2(
                    &raw mut actions.0,
                    fd.as_raw_fd(),
                    i32::try_from(destination).map_err(|_| io::ErrorKind::InvalidInput)?,
                )
            })?;
            duplicates.push(fd);
        }
        {
            let directory = string(directory.as_os_str())?;
            // SAFETY: initialized actions; the action copies the directory string.
            checked(unsafe {
                libc::posix_spawn_file_actions_addchdir_np(&raw mut actions.0, directory.as_ptr())
            })?;
        }
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        // glibc implements closefrom with close_range, with its own old-kernel
        // fallback. It executes after dup2, retaining exactly the allowlist.
        // SAFETY: initialized actions and a nonnegative descriptor boundary.
        checked(unsafe {
            libc::posix_spawn_file_actions_addclosefrom_np(&raw mut actions.0, minimum)
        })?;
        Ok((actions, duplicates))
    }

    #[allow(
        clippy::useless_conversion,
        reason = "libc spawn flag integer widths differ by target"
    )]
    fn attributes(&self) -> io::Result<Attributes> {
        let mut attributes = Attributes::new()?;
        // SAFETY: initialized attribute and signal sets; functions copy them.
        unsafe {
            let mut defaults: libc::sigset_t = std::mem::zeroed();
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&raw mut defaults);
            libc::sigemptyset(&raw mut mask);
            checked(libc::posix_spawnattr_setsigdefault(
                &raw mut attributes.0,
                &raw const defaults,
            ))?;
            checked(libc::posix_spawnattr_setsigmask(
                &raw mut attributes.0,
                &raw const mask,
            ))?;
            let mut flags = libc::POSIX_SPAWN_SETSIGDEF | libc::POSIX_SPAWN_SETSIGMASK;
            if self.session {
                #[cfg(target_vendor = "apple")]
                {
                    flags |= 0x0400;
                } // Darwin POSIX_SPAWN_SETSID, absent from libc bindings.
                #[cfg(target_os = "linux")]
                {
                    flags |= i32::from(libc::POSIX_SPAWN_SETSID);
                }
            } else if self.group {
                flags |= libc::POSIX_SPAWN_SETPGROUP;
                checked(libc::posix_spawnattr_setpgroup(&raw mut attributes.0, 0))?;
            }
            #[cfg(target_vendor = "apple")]
            {
                flags |= libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
            }
            checked(libc::posix_spawnattr_setflags(
                &raw mut attributes.0,
                libc::c_short::try_from(flags).expect("spawn flags fit a short"),
            ))?;
        }
        Ok(attributes)
    }
}

fn string(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
}
fn checked(code: i32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}
struct Actions(libc::posix_spawn_file_actions_t);
impl Actions {
    fn new() -> io::Result<Self> {
        // SAFETY: init writes the opaque output; ownership begins only on success.
        unsafe {
            let mut value = std::mem::MaybeUninit::uninit();
            checked(libc::posix_spawn_file_actions_init(value.as_mut_ptr()))?;
            Ok(Self(value.assume_init()))
        }
    }
}
impl Drop for Actions {
    fn drop(&mut self) {
        // SAFETY: this action list was initialized and is destroyed once.
        unsafe {
            libc::posix_spawn_file_actions_destroy(&raw mut self.0);
        }
    }
}
struct Attributes(libc::posix_spawnattr_t);
impl Attributes {
    fn new() -> io::Result<Self> {
        // SAFETY: init writes the opaque output; ownership begins only on success.
        unsafe {
            let mut value = std::mem::MaybeUninit::uninit();
            checked(libc::posix_spawnattr_init(value.as_mut_ptr()))?;
            Ok(Self(value.assume_init()))
        }
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: this attribute list was initialized and is destroyed once.
        unsafe {
            libc::posix_spawnattr_destroy(&raw mut self.0);
        }
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    fn null() -> File {
        File::options()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap()
    }

    #[test]
    fn relative_working_directories_and_path_entries_resolve_once() {
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
        let name = format!("target/spawn-paths-{}", std::process::id());
        let root = Directory(std::env::current_dir().unwrap().join(&name));
        std::fs::create_dir_all(root.0.join("child/bin")).unwrap();
        for directory in ["child", "child/bin"] {
            std::os::unix::fs::symlink("/bin/sh", root.0.join(directory).join("runner")).unwrap();
        }
        let null = null();
        for (program, path) in [
            ("runner", "."),
            ("runner", "bin"),
            ("runner", ""),
            ("./runner", ""),
            ("bin/runner", ""),
        ] {
            let mut command = Command::new(program);
            command
                .args(["-c", "exit 17"])
                .current_dir(std::path::Path::new(&name).join("child"))
                .env("PATH", path);
            assert_eq!(
                command.status().unwrap().code(),
                Some(17),
                "standard Command fixture: {program}, PATH={path:?}"
            );
            let mut child = SpawnPlan::new(&command, [&null, &null, &null])
                .spawn()
                .unwrap_or_else(|error| panic!("{program}, PATH={path:?}: {error}"));
            assert_eq!(child.wait().unwrap().code(), Some(17));
        }
    }

    #[test]
    fn a_waited_leader_remains_our_child_until_its_lease_is_released() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 17"]);
        let null = null();
        let mut child = SpawnPlan::new(&command, [&null, &null, &null])
            .spawn()
            .unwrap();
        let lease = super::super::ProcessLease::new(&child);
        let pid = child.id();
        assert_eq!(child.wait().unwrap().code(), Some(17));
        drop(child);
        assert!(observe(libc::P_PID, pid, true).unwrap().is_some());
        lease.signal(Signal::Kill).unwrap();
        drop(lease);
        assert_eq!(
            observe(libc::P_PID, pid, true).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn an_external_reaper_revokes_numeric_group_authority() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let null = null();
        let child = SpawnPlan::new(&command, [&null, &null, &null])
            .spawn()
            .unwrap();
        let lease = super::super::ProcessLease::new(&child);
        assert_eq!(
            // SAFETY: deliberate foreign-reaper fault injection for this test's child.
            unsafe { libc::waitpid(child.id().cast_signed(), std::ptr::null_mut(), 0) },
            child.id().cast_signed()
        );
        assert_eq!(
            lease.signal(Signal::Kill).unwrap_err().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn unlisted_descriptors_cannot_reach_an_executable() {
        use rustix::io::{FdFlags, fcntl_setfd};
        let (mut reader, writer) = io::pipe().unwrap();
        fcntl_setfd(&writer, FdFlags::empty()).unwrap();
        let null = null();
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            &format!(
                "if echo leaked >&{}; then exit 1; else exit 0; fi",
                writer.as_raw_fd()
            ),
        ]);
        let mut child = SpawnPlan::new(&command, [&null, &null, &null])
            .spawn()
            .unwrap();
        assert!(child.wait().unwrap().success());
        drop(writer);
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut reader, &mut bytes).unwrap();
        assert_eq!(bytes, [] as [u8; 0]);
    }

    #[test]
    fn ssh_keeps_the_callers_terminal_process_group() {
        use std::io::Read;
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "ps -o pgid= -p $$"]);
        let (mut child, input, mut output, _) =
            super::super::spawn_ssh(&mut command, super::super::Prompt::Terminal).unwrap();
        drop(input);
        let mut text = String::new();
        output.read_to_string(&mut text).unwrap();
        assert!(child.wait().unwrap().success());
        // SAFETY: getpgrp has no inputs or ownership effects.
        assert_eq!(text.trim().parse::<i32>().unwrap(), unsafe {
            libc::getpgrp()
        });
    }

    #[test]
    fn ssh_that_may_not_prompt_has_no_terminal_and_reports_on_its_own_pipe() {
        use std::io::Read;
        let mut command = Command::new("/bin/sh");
        command.args(["-c", r#"echo "$HOME" >&2; ps -o pgid= -p $$"#]);
        let (mut child, input, mut output, error) =
            super::super::spawn_ssh(&mut command, super::super::Prompt::Never).unwrap();
        drop(input);
        let mut text = String::new();
        output.read_to_string(&mut text).unwrap();
        let mut reported = String::new();
        error.unwrap().read_to_string(&mut reported).unwrap();
        assert!(child.wait().unwrap().success());
        // SAFETY: getpgrp has no inputs or ownership effects.
        assert_ne!(text.trim().parse::<i32>().unwrap(), unsafe {
            libc::getpgrp()
        });
        // ssh finds its keys and agent through the caller's environment.
        assert_eq!(reported.trim(), std::env::var("HOME").unwrap());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_nonblocking_exit_watch_cannot_change_the_owned_wait_contract() {
        use std::io::Write as _;
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "read -r line; exit 17"]);
        let null = null();
        let (input, mut writer) = io::pipe().unwrap();
        let input = File::from(OwnedFd::from(input));
        let mut child = SpawnPlan::new(&command, [&input, &null, &null])
            .spawn()
            .unwrap();
        let pid = rustix::process::Pid::from_raw(child.id().cast_signed()).unwrap();
        let watch = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::NONBLOCK) {
            Ok(watch) => watch,
            Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL) => return,
            Err(error) => panic!("{error}"),
        };
        Arc::get_mut(&mut child.0).unwrap().watch = Some(watch);
        let mut waiting = child.clone();
        let (done, result) = std::sync::mpsc::sync_channel(1);
        let task = std::thread::spawn(move || {
            done.send(waiting.wait()).unwrap();
        });
        let premature = result.recv_timeout(std::time::Duration::from_millis(50));
        writer.write_all(b"finish\n").unwrap();
        task.join().unwrap();
        assert!(matches!(
            premature,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(result.recv().unwrap().unwrap().code(), Some(17));
    }

    #[test]
    #[cfg(target_vendor = "apple")]
    fn an_already_exited_child_without_kqueue_reads_as_ended() {
        let null = null();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 17"]);
        let mut child = SpawnPlan::new(&command, [&null, &null, &null])
            .spawn()
            .unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(17));
        Arc::get_mut(&mut child.0).unwrap().watch.take();
        let watch = child.exit_watch().unwrap();
        let mut fds = [rustix::event::PollFd::new(
            &watch,
            rustix::event::PollFlags::IN,
        )];
        assert_eq!(
            rustix::event::poll(&mut fds, Some(&rustix::event::Timespec::default())).unwrap(),
            1
        );
        assert_eq!(child.wait().unwrap().code(), Some(17));
    }
}

#[cfg(all(
    target_os = "linux",
    any(not(target_env = "gnu"), all(test, not(miri)))
))]
#[path = "proc_linux.rs"]
mod linux;
