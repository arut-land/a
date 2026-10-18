//! Taking over the terminal and giving it back, on exit and on panic; and
//! the thread that turns its input into events.

use std::io::{self, Write};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, Once, PoisonError};
use std::thread;
use std::time::Duration;

use sys::tty::{self, Ready};

use super::Event;
use super::keys::Parser;
use super::render::Viewport;

/// Alternate screen, click and wheel reports (SGR encoding), bracketed
/// paste.
const FULL_ON: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h";
const FULL_OFF: &[u8] = b"\x1b[0m\x1b[?2004l\x1b[?1006l\x1b[?1000l\x1b[?1049l\x1b[?25h";
/// Inline keeps the terminal's own scrolling and selection: no mouse.
const INLINE_ON: &[u8] = b"\x1b[?2004h";
const INLINE_OFF: &[u8] = b"\x1b[0m\x1b[?2004l\x1b[?25h";

/// The mode to restore and the bytes that undo ours, while we hold the
/// terminal: a panic hook restores them too.
static HELD: Mutex<Option<(tty::Mode, &'static [u8])>> = Mutex::new(None);

pub struct Terminal(());

impl Terminal {
    pub fn enter(viewport: Viewport) -> io::Result<Self> {
        static HOOK: Once = Once::new();
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                previous(info);
            }));
        });
        let (on, off) = match viewport {
            Viewport::Full => (FULL_ON, FULL_OFF),
            Viewport::Inline => (INLINE_ON, INLINE_OFF),
        };
        let mode = tty::Mode::raw()?;
        *HELD.lock().unwrap_or_else(PoisonError::into_inner) = Some((mode, off));
        let mut out = io::stdout();
        out.write_all(on)?;
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
    if let Some((mode, off)) = held {
        let mut out = io::stdout();
        let _ = out.write_all(off);
        let _ = out.flush();
        mode.restore();
    }
}

/// How long the rest of an escape sequence may take to arrive before the
/// bytes count as typed (a lone Esc). Only waited for while bytes are
/// pending; otherwise the reader blocks with no timeout.
const ESC_WAIT: Duration = Duration::from_millis(30);

/// Reads the terminal on its own thread until stdin closes.
pub fn spawn_input(tx: Sender<Event>) -> io::Result<()> {
    let mut input = tty::Input::new()?;
    thread::spawn(move || {
        let mut parser = Parser::default();
        let mut buf = [0; 4096];
        let mut parsed = Vec::new();
        loop {
            let timeout = parser.pending().then_some(ESC_WAIT);
            match input.wait(&mut buf, timeout) {
                Ok(Ready::Bytes(n)) => parser.feed(&buf[..n], &mut parsed),
                Ok(Ready::TimedOut) => parser.flush(&mut parsed),
                Ok(Ready::Resized) => {
                    if tx.send(Event::Resized).is_err() {
                        return;
                    }
                }
                Ok(Ready::Closed) | Err(_) => {
                    let _ = tx.send(Event::InputClosed);
                    return;
                }
            }
            for input in parsed.drain(..) {
                if tx.send(Event::Input(input)).is_err() {
                    return;
                }
            }
        }
    });
    Ok(())
}
