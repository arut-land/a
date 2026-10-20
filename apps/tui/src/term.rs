//! Taking over the terminal and giving it back, on exit, on a panic and on
//! SIGTERM or SIGHUP; and the thread that turns its input and resizes into
//! the app's events.

use std::io::{self, Write};
use std::sync::{Mutex, Once, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

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
pub const ENTER: &[u8] = b"\x1b[?2004h";
pub const ENTER_FULL: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1006h";
pub const LEAVE: &[u8] = b"\x1b[0m\x1b[?2004l";
pub const LEAVE_FULL: &[u8] = b"\x1b[?1006l\x1b[?1000l\x1b[?1049l";

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

/// The mode first: it never blocks, where the escapes wait for a
/// terminal that reads them.
fn restore() {
    let held = HELD.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some((viewport, cooked)) = held {
        let _ = tcsetattr(rustix::stdio::stdin(), OptionalActions::Now, &cooked);
        let mut out = io::stdout().lock();
        let _ = out.write_all(LEAVE);
        if viewport == Viewport::Full {
            let _ = out.write_all(LEAVE_FULL);
        }
        let _ = out.write_all(b"\x1b[?25h");
        let _ = out.flush();
    }
}

/// How long a signal that ends the client waits for the terminal to take
/// the escapes that give it back.
const GIVE_BACK: Duration = Duration::from_millis(500);

/// Ends the client as `signal` would, the terminal given back first if it
/// takes it in time: a terminal nobody reads (its emulator hung, or gone
/// with a copy of its master left open) blocks every write, and must not
/// keep the client alive past a SIGTERM.
fn end(signal: Signal) -> ! {
    let (given, wait) = mpsc::channel();
    thread::spawn(move || {
        restore();
        let _ = given.send(());
    });
    let _ = wait.recv_timeout(GIVE_BACK);
    sys::reraise(signal)
}

/// The terminal's columns and rows.
pub fn size() -> io::Result<(u16, u16)> {
    let size = tcgetwinsize(rustix::stdio::stdout())?;
    Ok((size.ws_col, size.ws_row))
}

/// Reads the terminal and `signals` on a thread of its own. It blocks in
/// `poll` with no timeout: an idle client never wakes. What it posts goes
/// ahead of the daemon's backlog in the inbox: a key or a resize is folded
/// before a replay still queued. Once stdin is gone it goes on reading the
/// signals, which are caught for as long as the process lives: the client
/// may yet have a while to go, and a SIGTERM must end it all the same.
pub fn spawn_input(tx: client::Post<Event>, signals: Signals) {
    thread::spawn(move || {
        let stdin = rustix::stdio::stdin();
        let mut decoder = Decoder::default();
        let mut buf = [0; 4096];
        let mut open = true;
        loop {
            let mut fds = [
                PollFd::new(&signals, PollFlags::IN),
                PollFd::new(&stdin, PollFlags::IN),
            ];
            let polled = if open { &mut fds[..] } else { &mut fds[..1] };
            match poll(polled, None) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(_) => return,
            }
            let (signalled, input) = (!fds[0].revents().is_empty(), !fds[1].revents().is_empty());
            let mut resized = false;
            if signalled {
                for signal in signals.take() {
                    if signal == Signal::Resize {
                        resized = true;
                    } else {
                        end(signal);
                    }
                }
            }
            if resized && open && tx.send_first(Event::Resized).is_err() {
                open = false;
            }
            if input && open {
                let n = match rustix::io::read(stdin, &mut buf) {
                    Ok(n @ 1..) => n,
                    Err(Errno::INTR | Errno::AGAIN) => continue,
                    Ok(0) | Err(_) => {
                        open = false;
                        let _ = tx.send(Event::InputClosed);
                        continue;
                    }
                };
                // A full buffer says more is waiting: an ESC it ends with
                // may start a sequence.
                for input in decoder.feed(&buf[..n], n == buf.len()) {
                    if tx.send_first(Event::Input(input)).is_err() {
                        open = false;
                        break;
                    }
                }
            }
        }
    });
}
