//! Terminal modes, dimensions and input readiness behind one platform seam.

use std::io::{self, Write};

use crate::Signal;

pub use imp::{InputReader, RawMode, size, suspend};

pub const SUPPORTED: bool = cfg!(any(unix, windows));

pub enum Input {
    Signals(Vec<Signal>),
    Bytes(usize),
    Closed,
}

/// Unbuffered output keeps a frame in one write even when it holds newlines.
#[derive(Default)]
pub struct Stdout(());

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        imp::write_stdout(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
mod imp {
    use std::io;

    use rustix::event::{PollFd, PollFlags, poll};
    use rustix::io::Errno;
    use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};

    use super::Input;
    use crate::Signals;

    pub struct RawMode(Termios);

    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    impl RawMode {
        pub fn enter() -> io::Result<Self> {
            foreground()?;
            let stdin = rustix::stdio::stdin();
            let cooked = tcgetattr(stdin)?;
            let mut raw = cooked.clone();
            raw.make_raw();
            tcsetattr(stdin, OptionalActions::Now, &raw)?;
            Ok(Self(cooked))
        }

        pub fn restore_input(&self) -> io::Result<()> {
            self.restore()
        }

        pub fn restore(&self) -> io::Result<()> {
            // SAFETY: initialized signal sets; the previous thread mask is
            // restored before returning, including when tcsetattr fails.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                let mut old: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&raw mut set);
                libc::sigaddset(&raw mut set, libc::SIGTTOU);
                let error = libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, &raw mut old);
                if error != 0 {
                    return Err(io::Error::from_raw_os_error(error));
                }
                let result = tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &self.0);
                libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
                Ok(result?)
            }
        }
    }

    /// Never seize the foreground from the shell. A background resume stops
    /// again until the shell has assigned this job the controlling terminal.
    fn foreground() -> io::Result<()> {
        loop {
            let group = match rustix::termios::tcgetpgrp(rustix::stdio::stdin()) {
                Ok(group) => group,
                // A tty supplied without a controlling session has no job owner.
                Err(Errno::NOTTY) => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            if group == rustix::process::getpgrp() {
                return Ok(());
            }
            // SIGSTOP cannot be caught by our input thread's job-control handler.
            rustix::process::kill_process(
                rustix::process::getpid(),
                rustix::process::Signal::STOP,
            )?;
        }
    }

    pub fn suspend() -> io::Result<()> {
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::STOP)?;
        foreground()
    }

    pub fn size() -> io::Result<(u16, u16)> {
        let size = tcgetwinsize(rustix::stdio::stdout())?;
        Ok((size.ws_col, size.ws_row))
    }

    pub fn write_stdout(buf: &[u8]) -> io::Result<usize> {
        Ok(rustix::io::write(rustix::stdio::stdout(), buf)?)
    }

    pub struct InputReader {
        signals: Signals,
        open: bool,
    }

    impl InputReader {
        pub const fn new(signals: Signals) -> Self {
            Self {
                signals,
                open: true,
            }
        }

        /// A disconnected renderer still needs termination signals handled.
        pub fn close_input(&mut self) {
            self.open = false;
        }

        pub fn read(&mut self, buf: &mut [u8]) -> io::Result<Input> {
            let stdin = rustix::stdio::stdin();
            loop {
                let mut fds = [
                    PollFd::new(&self.signals, PollFlags::IN),
                    PollFd::new(&stdin, PollFlags::IN),
                ];
                let polled = if self.open {
                    &mut fds[..]
                } else {
                    &mut fds[..1]
                };
                match poll(polled, None) {
                    Ok(_) => {}
                    Err(Errno::INTR) => continue,
                    Err(e) => return Err(e.into()),
                }
                if !fds[0].revents().is_empty() {
                    return Ok(Input::Signals(self.signals.take()));
                }
                if self.open && !fds[1].revents().is_empty() {
                    match rustix::io::read(stdin, &mut *buf) {
                        Ok(n @ 1..) => return Ok(Input::Bytes(n)),
                        Err(Errno::INTR | Errno::AGAIN) => {}
                        Ok(0) | Err(_) => {
                            self.open = false;
                            return Ok(Input::Closed);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
#[path = "terminal/windows.rs"]
mod imp;
