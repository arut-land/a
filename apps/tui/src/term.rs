//! Taking over the terminal and giving it back, on exit, on a panic and on
//! SIGTERM or SIGHUP; and the thread that turns its input and resizes into
//! the app's events.

use std::io::{self, Write};
use std::sync::{Mutex, Once, PoisonError};
use std::thread;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::io::Errno;
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use sys::{Signal, Signals};

use super::Event;
use super::Viewport;
use super::keys::Decoder;

/// The signals the input thread handles: a resize, and the two that end
/// the client after the terminal is restored.
pub const SIGNALS: [Signal; 3] = [Signal::Resize, Signal::Terminate, Signal::Hangup];

/// Bracketed paste on; on the full screen also the alternate buffer and
/// click and wheel reports (SGR) only, not every motion, which would wake
/// the client whenever the pointer moves.
const ENTER: &[u8] = b"\x1b[?2004h";
const ENTER_FULL: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1006h";
const LEAVE: &[u8] = b"\x1b[0m\x1b[?2004l";
const LEAVE_FULL: &[u8] = b"\x1b[?1006l\x1b[?1000l\x1b[?1049l";

/// The viewport we hold the terminal for and its mode before: a panic hook
/// and the input thread restore it too.
static HELD: Mutex<Option<(Viewport, Termios)>> = Mutex::new(None);

pub struct Terminal(());

impl Terminal {
    /// Raw mode and bracketed paste; on the full screen also the alternate
    /// buffer and the mouse (inline keeps the terminal's own scrolling and
    /// selection).
    pub fn enter(viewport: Viewport) -> io::Result<Self> {
        static HOOK: Once = Once::new();
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                previous(info);
            }));
        });
        let stdin = rustix::stdio::stdin();
        let cooked = tcgetattr(stdin)?;
        let mut raw = cooked.clone();
        raw.make_raw();
        tcsetattr(stdin, OptionalActions::Now, &raw)?;
        *HELD.lock().unwrap_or_else(PoisonError::into_inner) = Some((viewport, cooked));
        let mut out = io::stdout().lock();
        if viewport == Viewport::Full {
            out.write_all(ENTER_FULL)?;
        }
        out.write_all(ENTER)?;
        out.flush()?;
        Ok(Self(()))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    let held = HELD.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some((viewport, cooked)) = held {
        let mut out = io::stdout().lock();
        let _ = out.write_all(LEAVE);
        if viewport == Viewport::Full {
            let _ = out.write_all(LEAVE_FULL);
        }
        let _ = out.write_all(b"\x1b[?25h");
        let _ = out.flush();
        let _ = tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &cooked);
    }
}

/// The terminal's columns and rows.
pub fn size() -> io::Result<(u16, u16)> {
    let size = tcgetwinsize(rustix::stdio::stdout())?;
    Ok((size.ws_col, size.ws_row))
}

/// Reads the terminal and `signals` on a thread of its own until stdin is
/// gone. It blocks in `poll` with no timeout: an idle client never wakes.
pub fn spawn_input(tx: client::Post<Event>, signals: Signals) {
    thread::spawn(move || {
        let stdin = rustix::stdio::stdin();
        let mut decoder = Decoder::default();
        let mut buf = [0; 4096];
        loop {
            let mut fds = [
                PollFd::new(&stdin, PollFlags::IN),
                PollFd::new(&signals, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(_) => break,
            }
            let (input, signalled) = (!fds[0].revents().is_empty(), !fds[1].revents().is_empty());
            let mut resized = false;
            if signalled {
                for signal in signals.take() {
                    if signal == Signal::Resize {
                        resized = true;
                    } else {
                        restore();
                        sys::reraise(signal);
                    }
                }
            }
            if resized && tx.send(Event::Resized).is_err() {
                return;
            }
            if input {
                let n = match rustix::io::read(stdin, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(Errno::INTR | Errno::AGAIN) => continue,
                    Err(_) => break,
                };
                // A full buffer says more is waiting: an ESC it ends with
                // may start a sequence.
                for input in decoder.feed(&buf[..n], n == buf.len()) {
                    if tx.send(Event::Input(input)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = tx.send(Event::InputClosed);
    });
}
