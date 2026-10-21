//! Signals as bytes on a pipe (the self-pipe trick), so a thread that polls
//! its input sees them with it, and no handler does more than one write.
//! Windows console control events wake a permanently owned manual-reset event.

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
        1
    }

    impl Signals {
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
            assert_eq!(
                unsafe { WaitForSingleObject(signals.as_handle().as_raw_handle(), 0) },
                0
            );
            assert_eq!(signals.take(), [Signal::Terminate]);
            assert!(signals.take().is_empty());
            // SAFETY: an unrequested callback must remain unhandled.
            assert_eq!(unsafe { control(CTRL_CLOSE_EVENT) }, 0);
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
    use std::sync::atomic::{AtomicI32, Ordering};

    use rustix::event::{PollFd, PollFlags, poll};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::io::{Errno, FdFlags, fcntl_setfd, read};

    use crate::Signal;
    use crate::proc::raw;

    /// Every signal [`Signals`] can report, to read a byte back as one.
    const ALL: [Signal; 5] = [
        Signal::Terminate,
        Signal::Kill,
        Signal::Interrupt,
        Signal::Hangup,
        Signal::Resize,
    ];

    /// The pipe's write end while a [`Signals`] lives, else -1.
    static PIPE: AtomicI32 = AtomicI32::new(-1);

    /// Caught signals, readable from [`AsFd::as_fd`] (poll it) and then
    /// [`Signals::take`]. One at a time per process; dropping it gives the
    /// signals their default action back.
    pub struct Signals {
        read: OwnedFd,
        write: OwnedFd,
        caught: Vec<Signal>,
    }

    impl Signals {
        pub fn catch(signals: &[Signal]) -> io::Result<Self> {
            let (read, write) = rustix::pipe::pipe()?;
            for fd in [&read, &write] {
                fcntl_setfd(fd, FdFlags::CLOEXEC)?;
                // A full pipe drops a signal rather than blocking the handler;
                // one byte already waiting says as much.
                fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;
            }
            PIPE.store(write.as_raw_fd(), Ordering::Release);
            let this = Self {
                read,
                write,
                caught: signals.to_vec(),
            };
            for &signal in signals {
                install(
                    signal,
                    on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
                )?;
            }
            Ok(this)
        }

        /// The signals that arrived since the last call, oldest first.
        pub fn take(&self) -> Vec<Signal> {
            let mut out = Vec::new();
            let mut buf = [0u8; 32];
            while let Ok(n @ 1..) = read(&self.read, &mut buf) {
                let raws = buf[..n].iter().map(|&b| i32::from(b));
                out.extend(
                    raws.filter_map(|byte| ALL.into_iter().find(|&s| raw(s).as_raw() == byte)),
                );
            }
            out
        }

        /// The signals that arrived since the last call, waited for if none
        /// has: for a thread that has nothing else to wait on.
        pub fn wait(&self) -> Vec<Signal> {
            loop {
                let got = self.take();
                if !got.is_empty() {
                    return got;
                }
                let mut fds = [PollFd::new(&self.read, PollFlags::IN)];
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
            self.read.as_fd()
        }
    }

    impl Drop for Signals {
        fn drop(&mut self) {
            for &signal in &self.caught {
                let _ = install(signal, libc::SIG_DFL);
            }
            let _ = PIPE.compare_exchange(
                self.write.as_raw_fd(),
                -1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }

    fn install(signal: Signal, handler: libc::sighandler_t) -> io::Result<()> {
        // SAFETY: a zeroed `sigaction` is a valid value of the C struct; every
        // field we rely on is set below.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler;
        // Other threads' interrupted calls restart rather than fail.
        action.sa_flags = libc::SA_RESTART;
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
        let fd = PIPE.load(Ordering::Acquire);
        if fd < 0 {
            return;
        }
        let saved = io::Error::last_os_error().raw_os_error();
        // Signal numbers fit a byte.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let byte = signal as u8;
        // SAFETY: write(2) is async-signal-safe; `byte` lives across the call
        // and `fd` is the pipe's write end, open while it is published.
        unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
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

        #[test]
        fn a_caught_signal_arrives_on_the_pipe() {
            let signals = Signals::catch(&[Signal::Resize]).unwrap();
            assert_eq!(signals.take(), []);
            crate::signal_process(std::process::id(), Signal::Resize).unwrap();
            // Delivered to some thread, soon.
            let mut got = Vec::new();
            for _ in 0..100 {
                got = signals.take();
                if !got.is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(got, [Signal::Resize]);
            crate::signal_process(std::process::id(), Signal::Resize).unwrap();
            assert_eq!(signals.wait(), [Signal::Resize]);
            drop(signals);
            assert_eq!(PIPE.load(Ordering::Relaxed), -1);
        }
    }
}
