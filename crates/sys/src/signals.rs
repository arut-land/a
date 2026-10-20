//! Signals as bytes on a pipe (the self-pipe trick), so a thread that polls
//! its input sees them with it, and no handler does more than one write.
//! Windows has console control events instead (`SetConsoleCtrlHandler`),
//! which arrive the same way once its terminal client lands.

pub use imp::{Signals, reraise};

#[cfg(windows)]
mod imp {
    use std::io;

    use crate::Signal;

    /// Caught console control events, as [`Signal`]s.
    pub struct Signals(());

    impl Signals {
        pub fn catch(_signals: &[Signal]) -> io::Result<Self> {
            Err(io::ErrorKind::Unsupported.into())
        }

        pub fn take(&self) -> Vec<Signal> {
            Vec::new()
        }

        pub fn wait(&self) -> Vec<Signal> {
            Vec::new()
        }
    }

    pub fn reraise(_signal: Signal) -> ! {
        std::process::exit(1)
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
