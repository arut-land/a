//! musl has no closefrom spawn action. The plan's fixed syscall sequence
//! supplies the same allowlist without running callbacks after fork.

use std::ffi::CStr;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(super) fn spawn(
    executable: &CStr,
    arguments: &[*mut libc::c_char],
    environment: &[*mut libc::c_char],
    directory: Option<&Path>,
    sources: &[OwnedFd],
    session: bool,
    group: bool,
) -> io::Result<libc::pid_t> {
    let directory = directory
        .map(|p| std::ffi::CString::new(p.as_os_str().as_bytes()))
        .transpose()
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    let boundary = i32::try_from(sources.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
    let descriptors: Vec<_> = sources
        .iter()
        .enumerate()
        .map(|(destination, source)| {
            Ok((
                source.as_raw_fd(),
                i32::try_from(destination).map_err(|_| io::ErrorKind::InvalidInput)?,
            ))
        })
        .collect::<io::Result<_>>()?;
    let (mut errors, writer) = io::pipe()?;
    // Keep the exec-error writer outside the plan's destination slots.
    // SAFETY: fcntl validates the borrowed descriptor and returns a new owner.
    let raw_writer = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, boundary) };
    if raw_writer < 0 {
        return Err(io::Error::last_os_error());
    }
    drop(writer);
    // SAFETY: fcntl returned an exclusively owned descriptor.
    let writer = unsafe { OwnedFd::from_raw_fd(raw_writer) };
    // The old-kernel close loop must include pre-existing descriptors even if
    // a caller lowered its hard limit after opening them.
    let max_open = std::fs::read_dir("/proc/self/fd")?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u64>().ok())
        .max()
        .unwrap_or(2);
    // SAFETY: getrlimit initializes this writable output.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: valid resource constant and output pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let ceiling =
        i32::try_from(limit.rlim_max.max(max_open + 1)).map_err(|_| io::ErrorKind::Unsupported)?;
    let mask = Mask::block()?;
    // SAFETY: the child executes only the fixed, async-signal-safe syscall
    // sequence below. It never allocates, locks, unwinds or drops Rust values.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: these immutable plan buffers and FDs were prepared before fork;
        // run terminates by execve or _exit, never returning to Rust cleanup.
        unsafe {
            Exec {
                executable,
                arguments,
                environment,
                directory: directory.as_deref(),
                descriptors: &descriptors,
                boundary,
                ceiling,
                error: writer.as_raw_fd(),
                session,
                group,
            }
            .run()
        }
    }
    drop(mask);
    let pending = Pending(pid);
    drop(writer);
    let mut bytes = [0; size_of::<i32>()];
    let mut read = 0;
    while read < bytes.len() {
        match errors.read(&mut bytes[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    if read == 0 {
        std::mem::forget(pending);
        return Ok(pid);
    }

    Err(if read == bytes.len() {
        io::Error::from_raw_os_error(i32::from_ne_bytes(bytes))
    } else {
        io::Error::other("incomplete child exec error")
    })
}

struct Exec<'a> {
    executable: &'a CStr,
    arguments: &'a [*mut libc::c_char],
    environment: &'a [*mut libc::c_char],
    directory: Option<&'a CStr>,
    descriptors: &'a [(i32, i32)],
    boundary: i32,
    ceiling: i32,
    error: i32,
    session: bool,
    group: bool,
}
impl Exec<'_> {
    unsafe fn run(&self) -> ! {
        // SAFETY: spawn prepared every buffer and descriptor before fork.
        // Only async-signal-safe calls run here; all failures use _exit.
        unsafe {
            for &(source, destination) in self.descriptors {
                if libc::dup2(source, destination) < 0 {
                    failed(self.error);
                }
            }
            if let Some(directory) = self.directory {
                if libc::chdir(directory.as_ptr()) < 0 {
                    failed(self.error);
                }
            }
            if self.session {
                if libc::setsid() < 0 {
                    failed(self.error);
                }
            } else if self.group && libc::setpgid(0, 0) < 0 {
                failed(self.error);
            }
            // Close all undesired FDs, retaining only the CLOEXEC error writer.
            let first = if self.error > self.boundary {
                libc::syscall(
                    libc::SYS_close_range,
                    self.boundary.cast_unsigned(),
                    (self.error - 1).cast_unsigned(),
                    0,
                )
            } else {
                0
            };
            let second = libc::syscall(
                libc::SYS_close_range,
                (self.error + 1).cast_unsigned(),
                u32::MAX,
                0,
            );
            if first < 0 || second < 0 {
                for descriptor in self.boundary..self.ceiling {
                    if descriptor != self.error {
                        libc::close(descriptor);
                    }
                }
            }
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut mask);
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&raw mut action.sa_mask);
            for signal in 1..=64 {
                if signal != libc::SIGKILL
                    && signal != libc::SIGSTOP
                    && libc::sigaction(signal, &raw const action, std::ptr::null_mut()) < 0
                {
                    // libc reserves a few real-time signals; EINVAL is benign.
                    if *libc::__errno_location() != libc::EINVAL {
                        failed(self.error);
                    }
                }
            }
            if libc::sigprocmask(libc::SIG_SETMASK, &raw const mask, std::ptr::null_mut()) < 0 {
                failed(self.error);
            }
            libc::execve(
                self.executable.as_ptr(),
                self.arguments.as_ptr().cast(),
                self.environment.as_ptr().cast(),
            );
            failed(self.error);
        }
    }
}

unsafe fn failed(writer: i32) -> ! {
    // SAFETY: errno is thread-local; write and _exit are async-signal-safe.
    unsafe {
        let code = *libc::__errno_location();
        let mut written = 0;
        let bytes = (&raw const code).cast::<u8>();
        while written < size_of::<i32>() {
            let n = libc::write(
                writer,
                bytes.add(written).cast(),
                size_of::<i32>() - written,
            );
            if n > 0 {
                written += n.cast_unsigned();
            } else if n == 0 || *libc::__errno_location() != libc::EINTR {
                break;
            }
        }
        libc::_exit(127)
    }
}

struct Pending(libc::pid_t);
impl Drop for Pending {
    fn drop(&mut self) {
        // SAFETY: this newly forked child has never been reaped or published.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
        // SAFETY: this guard is the only reaper for the failed launch.
        while unsafe { libc::waitpid(self.0, std::ptr::null_mut(), 0) } < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
}
struct Mask(libc::sigset_t);
impl Mask {
    fn block() -> io::Result<Self> {
        // SAFETY: both signal sets are writable; this changes only the spawning thread's mask.
        unsafe {
            let mut all = std::mem::zeroed();
            let mut old = std::mem::zeroed();
            libc::sigfillset(&raw mut all);
            let error = libc::pthread_sigmask(libc::SIG_BLOCK, &raw const all, &raw mut old);
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            Ok(Self(old))
        }
    }
}
impl Drop for Mask {
    fn drop(&mut self) {
        // SAFETY: the mask was saved on this same spawning thread.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const self.0, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs::File;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn musl_plans_close_unlisted_fds_and_return_native_exec_failures() {
        let null = File::options()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let sources: Vec<OwnedFd> = (0..3).map(|_| null.try_clone().unwrap().into()).collect();
        let (mut reader, writer) = io::pipe().unwrap();
        rustix::io::fcntl_setfd(&writer, rustix::io::FdFlags::empty()).unwrap();
        let program = CString::new("/bin/sh").unwrap();
        let arguments = [
            CString::new("sh").unwrap(),
            CString::new("-c").unwrap(),
            CString::new(format!(
                "if echo leaked >&{}; then exit 1; else exit 0; fi",
                writer.as_raw_fd()
            ))
            .unwrap(),
        ];
        let mut pointers: Vec<_> = arguments.iter().map(|s| s.as_ptr().cast_mut()).collect();
        pointers.push(std::ptr::null_mut());
        let pid = spawn(
            &program,
            &pointers,
            &[std::ptr::null_mut()],
            None,
            &sources,
            true,
            true,
        )
        .unwrap();
        let mut status = 0;
        // SAFETY: the test alone owns the child returned by this spawn.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        assert!(std::process::ExitStatus::from_raw(status).success());
        drop(writer);
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        let absent = CString::new("/a-process-executable-that-does-not-exist").unwrap();
        assert_eq!(
            spawn(
                &absent,
                &pointers,
                &[std::ptr::null_mut()],
                None,
                &sources,
                true,
                true
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::NotFound
        );
    }
}
