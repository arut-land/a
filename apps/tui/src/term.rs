//! Taking over the terminal and giving it back, on exit, on a panic and on
//! SIGTERM or SIGHUP; and the thread that turns its input and resizes into
//! the app's events.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

pub use sys::terminal::size;
use sys::terminal::{Input, InputReader, RawMode};
use sys::{Signal, Signals};

use super::Event;
use super::Viewport;
use super::keys::Decoder;

// Lifecycle controls must not wait behind a backlog of typed keys.
#[derive(Clone)]
pub struct Control {
    input: sys::terminal::InputControl,
    shared: Arc<SharedControl>,
}

struct SharedControl {
    epoch: AtomicU64,
    pending: Mutex<Pending>,
}

#[derive(Default)]
pub struct Pending {
    pub interrupt: Option<Interrupt>,
    pub resized: Option<sys::terminal::OutputInterrupt>,
    pub suspend: Option<sys::terminal::ResumeInput>,
    pub closed: Option<sys::terminal::OutputInterrupt>,
}

pub struct Interrupt {
    _output: sys::terminal::OutputInterrupt,
    pub repeat: bool,
}

#[derive(Clone, Copy)]
enum Request {
    Interrupt,
    Resize,
    Suspend,
    Closed,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.interrupt.is_none()
            && self.resized.is_none()
            && self.suspend.is_none()
            && self.closed.is_none()
    }
}

impl Control {
    fn new(input: sys::terminal::InputControl) -> Self {
        Self {
            input,
            shared: Arc::new(SharedControl {
                epoch: AtomicU64::new(0),
                pending: Mutex::new(Pending::default()),
            }),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::Acquire)
    }

    pub fn take(&self) -> Pending {
        std::mem::take(
            &mut *self
                .shared
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    pub fn cancel(&self) {
        drop(self.take());
    }

    fn request(&self, tx: &client::Post<Event>, request: Request) {
        let wake = {
            let mut pending = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let first = pending.is_empty();
            match request {
                Request::Interrupt => {
                    // Keys queued before Ctrl-C must not reappear after it clears input.
                    self.shared.epoch.fetch_add(1, Ordering::AcqRel);
                    if let Some(interrupt) = &mut pending.interrupt {
                        interrupt.repeat = true;
                    } else {
                        pending.interrupt = Some(Interrupt {
                            _output: sys::terminal::OutputInterrupt::acquire(),
                            repeat: false,
                        });
                    }
                }
                Request::Resize => {
                    pending
                        .resized
                        .get_or_insert_with(sys::terminal::OutputInterrupt::acquire);
                }
                Request::Closed => {
                    pending
                        .closed
                        .get_or_insert_with(sys::terminal::OutputInterrupt::acquire);
                }
                Request::Suspend => {
                    if pending.suspend.is_none() {
                        pending.suspend = self.input.pause();
                    }
                }
            }
            first && !pending.is_empty()
        };
        if wake {
            let _ = tx.send_first(Event::Control);
        }
    }
}

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
    // thread-exception: terminal-giveback
    thread::spawn(move || {
        restore();
        let _ = given.send(());
    });
    // thread-exception: terminal-deadline
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
pub fn spawn_input(tx: client::Post<Event>, signals: Signals) -> Control {
    let mut reader = InputReader::new(signals);
    let control = Control::new(reader.control());
    let input_control = control.clone();
    // thread-exception: terminal-input
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
                            input_control.request(&tx, Request::Interrupt);
                        } else {
                            end(signal);
                        }
                    }
                    if suspended {
                        request_suspend(&tx, &input_control);
                    }
                    if resized && open {
                        input_control.request(&tx, Request::Resize);
                    }
                }
                Input::Closed => {
                    open = false;
                    input_control.request(&tx, Request::Closed);
                }
                Input::Bytes { len, more } => {
                    for input in decoder.feed(&buf[..len], more) {
                        if input == super::keys::Input::Key(super::keys::Key::Ctrl('z')) {
                            request_suspend(&tx, &input_control);
                            continue;
                        }
                        if !send_input(&tx, &input_control, input) {
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

pub fn request_suspend(tx: &client::Post<Event>, control: &Control) {
    control.request(tx, Request::Suspend);
}

fn send_input(tx: &client::Post<Event>, control: &Control, input: super::keys::Input) -> bool {
    if input == super::keys::Input::Key(super::keys::Key::Ctrl('c')) {
        control.request(tx, Request::Interrupt);
        true
    } else {
        tx.send_first(Event::Input(input, control.epoch())).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::{Input as KeyInput, Key};
    use super::*;

    #[test]
    fn native_console_batches_preserve_vt_keys_and_pastes() {
        sys::terminal::testing::with_console(
            "term::tests::native_console_batches_preserve_vt_keys_and_pastes",
            || {
                let _raw = RawMode::enter().unwrap();
                let paste = "🙂 pasted text ".repeat(30);
                let text = format!("\x1b[A\x1b[B\x1bx\x1b[200~{paste}\x1b[201~");
                for capacity in [1, 2, 4096] {
                    let mut reader = InputReader::new(Signals::catch(&[Signal::Resize]).unwrap());
                    sys::terminal::testing::input(&text);
                    let mut decoder = Decoder::default();
                    let mut buf = vec![0; capacity];
                    let mut got = Vec::new();
                    let mut received = 0;
                    let mut resized = false;
                    while received < text.len() {
                        match reader.read(&mut buf).unwrap() {
                            Input::Bytes { len, more } => {
                                received += len;
                                got.extend(decoder.feed(&buf[..len], more));
                            }
                            Input::Signals(signals) => {
                                assert_eq!(signals, [Signal::Resize]);
                                resized = true;
                            }
                            Input::Closed => panic!("native console closed"),
                        }
                    }
                    assert!(resized);
                    assert_eq!(
                        got,
                        [
                            KeyInput::Key(Key::Up),
                            KeyInput::Key(Key::Down),
                            KeyInput::Key(Key::Alt('x')),
                            KeyInput::Paste(paste.clone()),
                        ],
                        "input capacity {capacity}"
                    );
                    sys::terminal::testing::input("\x1b");
                    loop {
                        if let Input::Bytes { len, more } = reader.read(&mut buf).unwrap() {
                            assert!(!more);
                            assert_eq!(decoder.feed(&buf[..len], more), [KeyInput::Key(Key::Esc)]);
                            break;
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn lifecycle_controls_bypass_a_full_typing_backlog() {
        let signals = Signals::catch(&[Signal::Resize]).unwrap();
        let reader = InputReader::new(signals);
        let control = Control::new(reader.control());
        let (post, inbox) = client::inbox(
            client::QueueBudget {
                bytes: 1024,
                requests: 2,
                control_reserve: 0,
            },
            |_| 1,
        );
        let epoch = control.epoch();
        for _ in 0..2 {
            assert!(
                post.send_first(Event::Input(KeyInput::Key(Key::Char('x')), epoch))
                    .is_ok()
            );
        }
        control.request(&post, Request::Interrupt);
        for _ in 0..1000 {
            control.request(&post, Request::Resize);
        }
        control.request(&post, Request::Interrupt);
        let pending = control.take();
        assert!(pending.interrupt.as_ref().unwrap().repeat);
        assert!(pending.resized.is_some());
        assert_ne!(control.epoch(), epoch, "queued typing survived Ctrl-C");
        assert!(control.take().is_empty());
        assert!(matches!(inbox.try_recv(), Some(Event::Input(_, old)) if old == epoch));
        assert!(sys::terminal::output_interrupted());
        drop(pending);
        assert!(!sys::terminal::output_interrupted());
    }
}
