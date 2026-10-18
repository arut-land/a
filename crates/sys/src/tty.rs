//! The terminal, for the full-screen client: raw mode, its size, and input
//! that wakes on bytes or a resize and on nothing else, so an idle client
//! never runs.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::OnceLock;
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::{Errno, FdFlags, fcntl_getfd, fcntl_setfd};
use rustix::termios::{self, OptionalActions, Termios};

/// The terminal mode stdin had before [`Mode::raw`], to put back.
#[derive(Clone)]
pub struct Mode(Termios);

impl Mode {
    /// Puts stdin's terminal in raw mode: bytes as typed, no echo, no
    /// signals from Ctrl-C, no output processing (lines end in `\r\n`).
    pub fn raw() -> io::Result<Self> {
        let stdin = io::stdin();
        let saved = termios::tcgetattr(&stdin)?;
        let mut raw = saved.clone();
        raw.make_raw();
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw)?;
        Ok(Self(saved))
    }

    /// Restores the saved mode; harmless to repeat, so a panic hook and a
    /// drop can both call it.
    pub fn restore(&self) {
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.0);
    }

    /// Has a SIGTERM or SIGHUP write `off` to stdout and restore this mode
    /// before the signal ends the process as it otherwise would. The first
    /// mode a process registers is the one restored.
    pub fn restore_on_signal(&self, off: &'static [u8]) -> io::Result<()> {
        if ON_SIGNAL.set((self.0.clone(), off)).is_err() {
            return Ok(());
        }
        for signal in [libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: the action only calls write(2), tcsetattr(3), signal(3)
            // and raise(3), all async-signal-safe, on fds 0 and 1 and data
            // set before it was registered; it never allocates, locks or
            // panics. With the default action back, the raised signal (held
            // until the handler returns) ends the process.
            unsafe {
                signal_hook_registry::register(signal, move || {
                    if let Some((mode, off)) = ON_SIGNAL.get() {
                        let (stdin, stdout) =
                            (BorrowedFd::borrow_raw(0), BorrowedFd::borrow_raw(1));
                        let _ = rustix::io::write(stdout, off);
                        let _ = termios::tcsetattr(stdin, OptionalActions::Now, mode);
                    }
                    libc::signal(signal, libc::SIG_DFL);
                    libc::raise(signal);
                })?;
            }
        }
        Ok(())
    }
}

/// What [`Mode::restore_on_signal`] puts back.
static ON_SIGNAL: OnceLock<(Termios, &'static [u8])> = OnceLock::new();

/// The terminal's size as (columns, rows), from stdout.
pub fn size() -> io::Result<(u16, u16)> {
    let size = termios::tcgetwinsize(io::stdout())?;
    Ok((size.ws_col, size.ws_row))
}

/// What [`Input::wait`] woke for.
#[derive(Debug, PartialEq, Eq)]
pub enum Ready {
    /// `n` bytes of input were read.
    Bytes(usize),
    Resized,
    TimedOut,
    /// stdin closed.
    Closed,
}

/// Stdin, and the read end of the pipe a SIGWINCH handler writes to.
pub struct Input {
    resized: &'static OwnedFd,
}

/// Registered once per process: the handler only writes a byte to a
/// non-blocking pipe, which is async-signal-safe.
static RESIZED: OnceLock<io::Result<OwnedFd>> = OnceLock::new();

fn resize_pipe() -> io::Result<OwnedFd> {
    let (read, write) = rustix::pipe::pipe()?;
    for fd in [&read, &write] {
        fcntl_setfd(fd, fcntl_getfd(fd)? | FdFlags::CLOEXEC)?;
        fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;
    }
    let signal = rustix::process::Signal::WINCH.as_raw();
    // SAFETY: the action runs in a signal handler and only calls write(2)
    // on a non-blocking pipe it owns, which is async-signal-safe; it never
    // allocates, locks or panics.
    unsafe {
        signal_hook_registry::register(signal, move || {
            let _ = rustix::io::write(&write, &[0]);
        })?;
    }
    Ok(read)
}

impl Input {
    pub fn new() -> io::Result<Self> {
        match RESIZED.get_or_init(resize_pipe) {
            Ok(resized) => Ok(Self { resized }),
            Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
        }
    }

    /// Blocks until input arrives (read into `buf`), the terminal is
    /// resized, or `timeout` passes. Without a timeout it waits forever.
    pub fn wait(&mut self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<Ready> {
        let stdin = io::stdin();
        let timeout = timeout.map(|t| Timespec {
            tv_sec: t.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: t.subsec_nanos().into(),
        });
        loop {
            let mut fds = [
                PollFd::new(self.resized, PollFlags::IN),
                PollFd::new(&stdin, PollFlags::IN),
            ];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(0) => return Ok(Ready::TimedOut),
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            if !fds[0].revents().is_empty() {
                let mut drain = [0; 64];
                while matches!(rustix::io::read(self.resized, &mut drain), Ok(n) if n > 0) {}
                return Ok(Ready::Resized);
            }
            if !fds[1].revents().is_empty() {
                return match rustix::io::read(stdin.as_fd(), &mut *buf) {
                    Ok(0) => Ok(Ready::Closed),
                    Ok(n) => Ok(Ready::Bytes(n)),
                    Err(Errno::INTR | Errno::AGAIN) => continue,
                    Err(e) => Err(e.into()),
                };
            }
        }
    }
}
