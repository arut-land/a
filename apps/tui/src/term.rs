//! Taking over the terminal and giving it back, on exit, on a panic and on
//! SIGTERM or SIGHUP; and the thread that turns its input and resizes into
//! the app's events.

use std::io::{self, Write};
use std::sync::{Mutex, Once, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

pub use sys::terminal::size;
use sys::terminal::{Input, InputReader, RawMode};
use sys::{Signal, Signals};

use super::Event;
use super::Viewport;
use super::keys::Decoder;

/// Terminal lifecycle requests are delivered before the daemon's backlog.
pub const SIGNALS: [Signal; 8] = [
    Signal::Resize,
    Signal::Terminate,
    Signal::Hangup,
    Signal::Interrupt,
    Signal::Suspend,
    Signal::Continue,
    Signal::BackgroundRead,
    Signal::BackgroundWrite,
];

/// Bracketed paste on; on the full screen also the alternate buffer and
/// click and wheel reports (SGR) only, not every motion, which would wake
/// the client whenever the pointer moves.
pub const ENTER: &[u8] = b"\x1b[?2004h";
pub const ENTER_FULL: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1006h";
pub const LEAVE: &[u8] = b"\x1b[0m\x1b[?2004l";
pub const LEAVE_FULL: &[u8] = b"\x1b[?1006l\x1b[?1000l\x1b[?1049l";

/// The viewport we hold the terminal for and its mode before: a panic hook
/// and the input thread restore it too.
static HELD: Mutex<Option<(Viewport, RawMode)>> = Mutex::new(None);

pub struct Terminal {
    viewport: Viewport,
}

impl Terminal {
    /// Raw mode and bracketed paste; on the full screen also the alternate
    /// buffer and the mouse (inline keeps the terminal's own scrolling and
    /// selection).
    pub fn enter(viewport: Viewport) -> io::Result<Self> {
        static HOOK: Once = Once::new();
        let cooked = RawMode::enter()?;
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                previous(info);
            }));
        });
        *HELD.lock().unwrap_or_else(PoisonError::into_inner) = Some((viewport, cooked));
        Ok(Self { viewport })
    }

    pub fn show(&self) -> io::Result<()> {
        let mut out = sys::terminal::Stdout::default();
        if self.viewport == Viewport::Full {
            out.write_all(ENTER_FULL)?;
        }
        out.write_all(ENTER)?;
        out.flush()?;
        Ok(())
    }
}

/// Called by the screen owner, so no frame races the cooked terminal.
pub fn suspend() -> io::Result<()> {
    if !sys::terminal::JOB_CONTROL {
        return Ok(());
    }
    let viewport = HELD
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(viewport, _)| *viewport);
    if let Some(viewport) = viewport {
        restore();
        sys::terminal::suspend()?;
        // Ownership remains with the original Terminal guard.
        std::mem::forget(Terminal::enter(viewport)?);
    }
    Ok(())
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
        let _output = sys::terminal::OutputGuard::enter();
        let _ = cooked.restore_input();
        let mut out = sys::terminal::Stdout::restoring();
        let _ = out.write_all(LEAVE);
        if viewport == Viewport::Full {
            let _ = out.write_all(LEAVE_FULL);
        }
        let _ = out.write_all(b"\x1b[?25h");
        if viewport == Viewport::Inline {
            let _ = out.write_all(b"\r\n");
        }
        let _ = out.flush();
        let _ = cooked.restore();
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
    sys::terminal::restore();
    sys::reraise(signal)
}

/// Reads the terminal and `signals` on a thread of its own. It blocks in
/// a native wait with no timeout: an idle client never wakes. What it posts goes
/// ahead of the daemon's backlog in the inbox: a key or a resize is folded
/// before a replay still queued. Once stdin is gone it goes on reading the
/// signals, which are caught for as long as the process lives: the client
/// may yet have a while to go, and a SIGTERM must end it all the same.
pub fn spawn_input(tx: client::Post<Event>, signals: Signals) -> sys::terminal::InputControl {
    let mut reader = InputReader::new(signals);
    let control = reader.control();
    let input_control = control.clone();
    thread::spawn(move || {
        let mut decoder = Decoder::default();
        let mut buf = [0; 4096];
        let mut open = true;
        while let Ok(input) = reader.read(&mut buf) {
            match input {
                Input::Signals(signals) => {
                    let mut resized = false;
                    let mut suspended = false;
                    for signal in signals {
                        if matches!(signal, Signal::Resize | Signal::Continue) {
                            resized = true;
                        } else if matches!(
                            signal,
                            Signal::Suspend | Signal::BackgroundRead | Signal::BackgroundWrite
                        ) {
                            suspended = true;
                        } else if signal == Signal::Interrupt {
                            let _ = send_input(
                                &tx,
                                super::keys::Input::Key(super::keys::Key::Ctrl('c')),
                            );
                        } else {
                            end(signal);
                        }
                    }
                    if suspended {
                        request_suspend(&tx, &input_control);
                    }
                    if resized
                        && open
                        && tx
                            .send_first(Event::Resized(sys::terminal::OutputInterrupt::acquire()))
                            .is_err()
                    {
                        open = false;
                        reader.close_input();
                    }
                }
                Input::Closed => {
                    open = false;
                    let _ = tx.send(Event::InputClosed(sys::terminal::OutputInterrupt::acquire()));
                }
                Input::Bytes(n) => {
                    // A full buffer may end in the start of an escape sequence.
                    for input in decoder.feed(&buf[..n], n == buf.len()) {
                        if input == super::keys::Input::Key(super::keys::Key::Ctrl('z')) {
                            request_suspend(&tx, &input_control);
                            continue;
                        }
                        if !send_input(&tx, input) {
                            open = false;
                            reader.close_input();
                            break;
                        }
                    }
                }
            }
        }
    });
    control
}

// Keep the reader away from cooked input while the shell owns the terminal.
pub fn request_suspend(tx: &client::Post<Event>, control: &sys::terminal::InputControl) {
    if let Some(resume) = control.pause() {
        let _ = tx.send_first(Event::Suspend(resume));
    }
}

fn send_input(tx: &client::Post<Event>, input: super::keys::Input) -> bool {
    let output = (input == super::keys::Input::Key(super::keys::Key::Ctrl('c')))
        .then(sys::terminal::OutputInterrupt::acquire);
    tx.send_first(Event::Input(input, output)).is_ok()
}
