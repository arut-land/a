//! Pending signal bits with a wake source that outlives every handler.
//! An exclusive lease prevents competing consumers from stealing shutdown.
//! Windows console control events wake a permanently owned manual-reset event.

pub(crate) use imp::Wake;
pub use imp::{Signals, reraise};

#[cfg(windows)]
mod imp {
    use crate::Signal;
    use std::io;
    use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, OwnedHandle};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::Threading::{
        INFINITE, ResetEvent, SetEvent, WaitForSingleObject,
    };

    struct Source {
        event: OwnedHandle,
        pending: AtomicU32,
        requested: AtomicU32,
        active: AtomicBool,
    }
    // Handler invocations may outlive a Signals. Process-lifetime ownership
    // prevents a closing event handle from being reused under the callback.
    static SOURCE: OnceLock<Source> = OnceLock::new();

    pub struct Signals(&'static Source);

    #[derive(Clone, Copy)]
    pub(crate) struct Wake(&'static Source);

    impl Wake {
        pub(crate) fn wake(self) {
            // SAFETY: the source owns its event for the process lifetime.
            unsafe {
                SetEvent(self.0.event.as_raw_handle());
            }
        }
    }

    impl AsHandle for Signals {
        fn as_handle(&self) -> BorrowedHandle<'_> {
            self.0.event.as_handle()
        }
    }

    fn bit(signal: Signal) -> u32 {
        match signal {
            Signal::Interrupt => 1,
            Signal::Terminate => 2,
            Signal::Hangup => 4,
            _ => 0,
        }
    }

    unsafe extern "system" fn control(event: u32) -> i32 {
        let signal = match event {
            CTRL_C_EVENT => Signal::Interrupt,
            CTRL_BREAK_EVENT => Signal::Terminate,
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => Signal::Hangup,
            _ => return 0,
        };
        let Some(source) = SOURCE.get() else {
            return 0;
        };
        let bit = bit(signal);
        if source.requested.load(Ordering::Acquire) & bit == 0 {
            return 0;
        }
        source.pending.fetch_or(bit, Ordering::Release);
        // SAFETY: SOURCE owns the event for the process lifetime.
        unsafe { SetEvent(source.event.as_raw_handle()) };
        if signal == Signal::Hangup {
            // Windows terminates the process when a close/logoff handler
            // returns; waking the renderer alone cannot guarantee cleanup.
            crate::terminal::restore();
        }
        1
    }

    impl Signals {
        pub(crate) fn waker(&self) -> Wake {
            Wake(self.0)
        }

        pub fn catch(signals: &[Signal]) -> io::Result<Self> {
            if SOURCE.get().is_none() {
                let source = Source {
                    event: crate::windows::event()?,
                    pending: AtomicU32::new(0),
                    requested: AtomicU32::new(0),
                    active: AtomicBool::new(false),
                };
                let _ = SOURCE.set(source);
            }
            let source = SOURCE.get().unwrap();
            if source.active.swap(true, Ordering::AcqRel) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            let this = Self(source);
            this.take();
            source.requested.store(
                signals.iter().fold(0, |bits, signal| bits | bit(*signal)),
                Ordering::Release,
            );
            // SAFETY: callback uses only atomics and its permanently owned event.
            crate::windows::ok(unsafe { SetConsoleCtrlHandler(Some(control), 1) })?;
            Ok(this)
        }

        pub fn take(&self) -> Vec<Signal> {
            // Reset before draining, so a concurrent callback always leaves
            // either a pending bit in this drain or a signaled event behind.
            // SAFETY: the process-lifetime event remains open.
            unsafe { ResetEvent(self.0.event.as_raw_handle()) };
            let bits = self.0.pending.swap(0, Ordering::AcqRel);
            [Signal::Interrupt, Signal::Terminate, Signal::Hangup]
                .into_iter()
                .filter(|signal| bits & bit(*signal) != 0)
                .collect()
        }

        pub fn wait(&self) -> Vec<Signal> {
            // SAFETY: this event stays owned throughout the wait.
            unsafe { WaitForSingleObject(self.0.event.as_raw_handle(), INFINITE) };
            self.take()
        }
    }

    impl Drop for Signals {
        fn drop(&mut self) {
            self.0.requested.store(0, Ordering::Release);
            // SAFETY: removes precisely this process's registered callback.
            unsafe { SetConsoleCtrlHandler(Some(control), 0) };
            self.0.active.store(false, Ordering::Release);
        }
    }

    pub fn reraise(_signal: Signal) -> ! {
        std::process::exit(1)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn console_events_wake_the_daemon_and_drain_once() {
            let signals = Signals::catch(&[Signal::Terminate, Signal::Interrupt]).unwrap();
            // SAFETY: exercises the same scalar callback input the OS supplies.
            assert_eq!(unsafe { control(CTRL_BREAK_EVENT) }, 1);
            // SAFETY: the source owns this event for the entire test.
            let status = unsafe { WaitForSingleObject(signals.as_handle().as_raw_handle(), 0) };
            assert_eq!(status, 0);
            assert_eq!(signals.take(), [Signal::Terminate]);
            assert_eq!(signals.take(), []);
            // SAFETY: an unrequested callback must remain unhandled.
            assert_eq!(unsafe { control(CTRL_CLOSE_EVENT) }, 0);
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use rustix::event::{PollFd, PollFlags, poll};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::io::{Errno, FdFlags, fcntl_setfd, read};

    use crate::Signal;
    use crate::proc::raw;

    const ALL: [Signal; 8] = [
        Signal::Terminate,
        Signal::Interrupt,
        Signal::Hangup,
        Signal::Resize,
        Signal::Suspend,
        Signal::Continue,
        Signal::BackgroundRead,
        Signal::BackgroundWrite,
    ];

    struct Source {
        read: OwnedFd,
        write: OwnedFd,
        pending: AtomicU32,
        requested: AtomicU32,
        active: AtomicBool,
    }

    // A handler already in flight may write after its lease is dropped.
    // Keeping both ends open prevents fd reuse and SIGPIPE in that race.
    static SOURCE: OnceLock<Source> = OnceLock::new();

    struct SignalLease {
        source: &'static Source,
        caught: Vec<Signal>,
    }

    pub struct Signals(SignalLease);

    #[derive(Clone, Copy)]
    pub(crate) struct Wake(&'static Source);

    impl Wake {
        pub(crate) fn wake(self) {
            let _ = rustix::io::write(&self.0.write, &[1]);
        }
    }

    fn bit(signal: Signal) -> u32 {
        match signal {
            Signal::Terminate => 1,
            Signal::Interrupt => 2,
            Signal::Hangup => 4,
            Signal::Resize => 8,
            Signal::Suspend => 16,
            Signal::Continue => 32,
            Signal::BackgroundRead => 64,
            Signal::BackgroundWrite => 128,
            Signal::Kill => 0,
        }
    }

    impl Signals {
        pub(crate) fn waker(&self) -> Wake {
            Wake(self.0.source)
        }

        pub fn catch(signals: &[Signal]) -> io::Result<Self> {
            if signals.contains(&Signal::Kill) {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            if SOURCE.get().is_none() {
                let (read, write) = rustix::pipe::pipe()?;
                for fd in [&read, &write] {
                    fcntl_setfd(fd, FdFlags::CLOEXEC)?;
                    fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;
                }
                let _ = SOURCE.set(Source {
                    read,
                    write,
                    pending: AtomicU32::new(0),
                    requested: AtomicU32::new(0),
                    active: AtomicBool::new(false),
                });
            }
            let source = SOURCE.get().unwrap();
            if source.active.swap(true, Ordering::AcqRel) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            let mut this = Self(SignalLease {
                source,
                caught: Vec::new(),
            });
            this.take();
            source.requested.store(
                signals.iter().fold(0, |bits, signal| bits | bit(*signal)),
                Ordering::Release,
            );
            for &signal in signals {
                install(
                    signal,
                    on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
                )?;
                this.0.caught.push(signal);
            }
            Ok(this)
        }

        /// Coalesced signals since the last drain, with termination first.
        pub fn take(&self) -> Vec<Signal> {
            let mut buf = [0u8; 256];
            for _ in 0..64 {
                match read(&self.0.source.read, &mut buf) {
                    Ok(1..) | Err(Errno::INTR) => {}
                    _ => break,
                }
            }
            // Drain the wake pipe before the bits: a concurrent handler
            // leaves either a bit in this batch or a wake for the next one.
            let bits = self.0.source.pending.swap(0, Ordering::AcqRel)
                & self.0.source.requested.load(Ordering::Acquire);
            ALL.into_iter()
                .filter(|signal| bits & bit(*signal) != 0)
                .collect()
        }

        /// The signals that arrived since the last call, waited for if none
        /// has: for a thread that has nothing else to wait on.
        pub fn wait(&self) -> Vec<Signal> {
            loop {
                let got = self.take();
                if !got.is_empty() {
                    return got;
                }
                let mut fds = [PollFd::new(&self.0.source.read, PollFlags::IN)];
                if let Err(e) = poll(&mut fds, None)
                    && e != Errno::INTR
                {
                    return Vec::new();
                }
            }
        }
    }

    impl AsFd for Signals {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.source.read.as_fd()
        }
    }

    impl Drop for SignalLease {
        fn drop(&mut self) {
            self.source.requested.store(0, Ordering::Release);
            for &signal in &self.caught {
                let _ = install(signal, libc::SIG_DFL);
            }
            self.source.active.store(false, Ordering::Release);
        }
    }

    fn install(signal: Signal, handler: libc::sighandler_t) -> io::Result<()> {
        // SAFETY: a zeroed `sigaction` is a valid value of the C struct; every
        // field we rely on is set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler;
        // Other threads' interrupted calls restart rather than fail.
        action.sa_flags = if matches!(signal, Signal::BackgroundRead | Signal::BackgroundWrite) {
            0
        } else {
            libc::SA_RESTART
        };
        // SAFETY: `sa_mask` is a valid, writable signal set; `action` is fully
        // initialized and the handler is async-signal-safe (`on_signal`) or
        // `SIG_DFL`.
        let ok = unsafe {
            libc::sigemptyset(&raw mut action.sa_mask);
            libc::sigaction(
                raw(signal).as_raw(),
                &raw const action,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    extern "C" fn on_signal(signal: libc::c_int) {
        let Some(source) = SOURCE.get() else {
            return;
        };
        let Some(signal) = ALL.into_iter().find(|&s| raw(s).as_raw() == signal) else {
            return;
        };
        let bit = bit(signal);
        if source.requested.load(Ordering::Acquire) & bit == 0 {
            return;
        }
        source.pending.fetch_or(bit, Ordering::Release);
        let saved = io::Error::last_os_error().raw_os_error();
        let byte = 1u8;
        // SAFETY: write is async-signal-safe and both pipe ends are owned
        // for the process lifetime. Pending bits survive a full pipe.
        unsafe { libc::write(source.write.as_raw_fd(), (&raw const byte).cast(), 1) };
        if let Some(errno) = saved {
            set_errno(errno);
        }
    }

    #[cfg(target_vendor = "apple")]
    fn set_errno(value: i32) {
        // SAFETY: `__error` returns this thread's errno slot.
        unsafe { *libc::__error() = value };
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn set_errno(value: i32) {
        // SAFETY: `__errno_location` returns this thread's errno slot.
        unsafe { *libc::__errno_location() = value };
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    fn set_errno(_value: i32) {}

    /// Ends the process as `signal` would have, had it not been caught.
    pub fn reraise(signal: Signal) -> ! {
        let _ = install(signal, libc::SIG_DFL);
        // SAFETY: raise(3) with a valid signal number; its default action ends
        // the process.
        unsafe { libc::raise(raw(signal).as_raw()) };
        std::process::exit(128 + raw(signal).as_raw())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::process::ExitStatusExt;

        // libtest can run tests concurrently; signal ownership is per process.
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

        #[test]
        fn every_spawn_path_resets_signal_masks_and_ignored_dispositions() {
            use std::process::{Command, Stdio};
            const TEST: &str = "signals::imp::tests::every_spawn_path_resets_signal_masks_and_ignored_dispositions";
            let mode = std::env::var("A_SYS_SPAWN_STATE").ok();
            if mode.is_none() {
                // Dispositions are process-wide: isolate the ignored-signal
                // fixture rather than changing other libtest threads' handlers.
                let output = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env("A_SYS_SPAWN_STATE", "spawn")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stdout)
                );
                return;
            }
            if mode.as_deref() == Some("inspect") {
                // SAFETY: valid writable buffers; null set queries the mask
                // and null action queries a disposition without modifying it.
                unsafe {
                    let mut mask: libc::sigset_t = std::mem::zeroed();
                    assert_eq!(
                        libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &raw mut mask),
                        0
                    );
                    for signal in 1..128 {
                        assert_ne!(
                            libc::sigismember(&raw const mask, signal),
                            1,
                            "blocked {signal}"
                        );
                    }
                    for signal in ALL {
                        let mut action: libc::sigaction = std::mem::zeroed();
                        assert_eq!(
                            libc::sigaction(
                                raw(signal).as_raw(),
                                std::ptr::null(),
                                &raw mut action
                            ),
                            0
                        );
                        assert_eq!(action.sa_sigaction, libc::SIG_DFL, "{signal:?}");
                    }
                }
                return;
            }
            assert_eq!(mode.as_deref(), Some("spawn"));
            // SAFETY: the valid sets/actions affect only this isolated test
            // process. Blocking is thread-local and inherited by each child.
            unsafe {
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigfillset(&raw mut mask);
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_SETMASK, &raw const mask, std::ptr::null_mut()),
                    0
                );
                let mut ignored: libc::sigaction = std::mem::zeroed();
                ignored.sa_sigaction = libc::SIG_IGN;
                libc::sigemptyset(&raw mut ignored.sa_mask);
                for signal in ALL {
                    assert_eq!(
                        libc::sigaction(
                            raw(signal).as_raw(),
                            &raw const ignored,
                            std::ptr::null_mut()
                        ),
                        0
                    );
                }
            }
            let command = || {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", TEST, "--nocapture", "--"])
                    .env("A_SYS_SPAWN_STATE", "inspect")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                command
            };
            let (mut child, input, output) = crate::spawn_stdio(&mut command()).unwrap();
            drop(input);
            let status = child.wait().unwrap();
            drop(output);
            assert!(status.success(), "stdio child inherited signal state");
            let dir = std::env::temp_dir().join(format!("a-c02-spawn-{}", std::process::id()));
            std::fs::create_dir_all(dir.join("run")).unwrap();
            let home = crate::Home::from(dir.clone());
            let output = std::fs::File::create(dir.join("output")).unwrap();
            let mut child = crate::spawn_group(&mut command(), output).unwrap();
            assert!(
                child.wait().unwrap().success(),
                "group child inherited signal state"
            );
            let lock = std::fs::File::create(home.daemon_pid_file()).unwrap();
            crate::fs::try_lock(&lock).unwrap();
            let mut child = crate::spawn_daemon(&mut command(), &home, lock).unwrap();
            assert!(
                child.wait().unwrap().success(),
                "daemon child inherited signal state"
            );
            home.endpoint().remove();
            std::fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn tool_children_can_receive_termination_when_the_parent_blocks_it() {
            let _serial = SERIAL.lock().unwrap();
            let status = std::thread::spawn(|| {
                let _signals = Signals::catch(&[Signal::Terminate]).unwrap();
                // SAFETY: valid writable signal sets; only this test thread's mask changes.
                let old = unsafe {
                    let mut blocked: libc::sigset_t = std::mem::zeroed();
                    let mut old: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&raw mut blocked);
                    libc::sigaddset(&raw mut blocked, libc::SIGTERM);
                    assert_eq!(
                        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const blocked, &raw mut old),
                        0
                    );
                    old
                };
                let mut command = std::process::Command::new("/bin/sh");
                command.args(["-c", "kill -TERM $$; exit 42"]);
                let spawned = crate::spawn_stdio(&mut command);
                // SAFETY: restore the valid mask saved above before asserting results.
                assert_eq!(
                    // SAFETY: old is the valid mask saved on this thread above.
                    unsafe {
                        libc::pthread_sigmask(
                            libc::SIG_SETMASK,
                            &raw const old,
                            std::ptr::null_mut(),
                        )
                    },
                    0
                );
                let (mut child, input, output) = spawned.unwrap();
                drop((input, output));
                child.wait().unwrap()
            })
            .join()
            .unwrap();
            assert_eq!(
                status.signal(),
                Some(libc::SIGTERM),
                "child exited as {status}"
            );
        }

        #[test]
        fn signal_registration_is_exclusive() {
            let _serial = SERIAL.lock().unwrap();
            let signals = Signals::catch(&[Signal::Resize]).unwrap();
            assert!(matches!(Signals::catch(&[Signal::Terminate]), Err(e)
                if e.kind() == io::ErrorKind::AlreadyExists));
            on_signal(raw(Signal::Resize).as_raw());
            assert_eq!(signals.wait(), [Signal::Resize]);
            drop(signals);
            let next = Signals::catch(&[Signal::Terminate]).unwrap();
            // A handler from the previous lease may finish after reacquisition.
            next.0
                .source
                .pending
                .fetch_or(bit(Signal::Resize), Ordering::Release);
            assert_eq!(next.take(), []);
            drop(next);
            assert!(Signals::catch(&[Signal::Kill]).is_err());
        }

        #[test]
        fn termination_survives_a_full_signal_pipe() {
            let _serial = SERIAL.lock().unwrap();
            let signals = Signals::catch(&[Signal::Resize, Signal::Terminate]).unwrap();
            let source = signals.0.source;
            let buf = [1u8; 1024];
            while rustix::io::write(&source.write, &buf).is_ok() {}
            on_signal(raw(Signal::Terminate).as_raw());
            assert_eq!(signals.take(), [Signal::Terminate]);
            assert_eq!(signals.take(), []);
        }
    }
}
