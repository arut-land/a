//! Terminal modes, dimensions and input readiness behind one platform seam.

use std::io::{self, Write};

use crate::Signal;

pub use imp::{InputReader, RawMode, size};

pub const SUPPORTED: bool = cfg!(unix);

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

    impl RawMode {
        pub fn enter() -> io::Result<Self> {
            let stdin = rustix::stdio::stdin();
            let cooked = tcgetattr(stdin)?;
            let mut raw = cooked.clone();
            raw.make_raw();
            tcsetattr(stdin, OptionalActions::Now, &raw)?;
            Ok(Self(cooked))
        }

        pub fn restore(&self) -> io::Result<()> {
            Ok(tcsetattr(
                rustix::stdio::stdin(),
                OptionalActions::Now,
                &self.0,
            )?)
        }
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

#[cfg(not(unix))]
mod imp {
    use std::io;

    use super::Input;
    use crate::Signals;

    pub struct RawMode(());

    impl RawMode {
        pub fn enter() -> io::Result<Self> {
            Err(io::ErrorKind::Unsupported.into())
        }

        pub fn restore(&self) -> io::Result<()> {
            Ok(())
        }
    }

    pub fn size() -> io::Result<(u16, u16)> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn write_stdout(_: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub struct InputReader;

    impl InputReader {
        pub fn new(_: Signals) -> Self {
            Self
        }

        pub fn close_input(&mut self) {}

        pub fn read(&mut self, _: &mut [u8]) -> io::Result<Input> {
            Err(io::ErrorKind::Unsupported.into())
        }
    }
}
