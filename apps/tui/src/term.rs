//! Taking over the terminal and giving it back, on exit, on a panic and on
//! SIGTERM or SIGHUP; and the thread that turns its events into the app's.

use std::fmt;
use std::io::{self, Write};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, Once, PoisonError};
use std::thread;

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{Command, cursor, queue, style};

use super::Event;
use super::keys::Input;
use super::render::Viewport;

/// Click and wheel reports only (SGR encoding): crossterm's
/// `EnableMouseCapture` also asks for every motion, which would wake the
/// client whenever the pointer moves.
struct ClickAndWheel(bool);

impl Command for ClickAndWheel {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str(if self.0 {
            "\x1b[?1000h\x1b[?1006h"
        } else {
            "\x1b[?1006l\x1b[?1000l"
        })
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        if self.0 {
            EnableMouseCapture.execute_winapi()
        } else {
            DisableMouseCapture.execute_winapi()
        }
    }
}

/// The viewport we hold the terminal for: a panic hook and the signal
/// thread restore it too.
static HELD: Mutex<Option<Viewport>> = Mutex::new(None);

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
            #[cfg(unix)]
            restore_on_signal();
        });
        terminal::enable_raw_mode()?;
        *HELD.lock().unwrap_or_else(PoisonError::into_inner) = Some(viewport);
        let mut out = io::stdout();
        if viewport == Viewport::Full {
            queue!(out, EnterAlternateScreen, ClickAndWheel(true))?;
        }
        queue!(out, EnableBracketedPaste)?;
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
    if let Some(viewport) = held {
        let mut out = io::stdout();
        let _ = queue!(out, style::ResetColor, DisableBracketedPaste);
        if viewport == Viewport::Full {
            let _ = queue!(out, ClickAndWheel(false), LeaveAlternateScreen);
        }
        let _ = queue!(out, cursor::Show);
        let _ = out.flush();
        let _ = terminal::disable_raw_mode();
    }
}

/// A SIGTERM or SIGHUP restores the terminal on a thread of its own, then
/// ends the process as the signal would have.
#[cfg(unix)]
fn restore_on_signal() {
    use signal_hook::consts::{SIGHUP, SIGTERM};
    use signal_hook::iterator::Signals;

    let Ok(mut signals) = Signals::new([SIGTERM, SIGHUP]) else {
        return;
    };
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            restore();
            let _ = signal_hook::low_level::emulate_default_handler(signal);
        }
    });
}

/// Reads the terminal on its own thread until it is gone. It blocks with
/// no timeout: an idle client never wakes.
pub fn spawn_input(tx: Sender<Event>, viewport: Viewport) {
    thread::spawn(move || {
        loop {
            let sent = match event::read() {
                Ok(event::Event::Resize(..)) => {
                    // Inline: the terminal reflowed its lines, so ask where
                    // the cursor (and with it the viewport) ended up.
                    let row = (viewport == Viewport::Inline)
                        .then(|| cursor::position().ok().map(|(_, row)| row))
                        .flatten();
                    tx.send(Event::Resized(row))
                }
                Ok(event) => match Input::from_event(event) {
                    Some(input) => tx.send(Event::Input(input)),
                    None => continue,
                },
                Err(_) => {
                    let _ = tx.send(Event::InputClosed);
                    return;
                }
            };
            if sent.is_err() {
                return;
            }
        }
    });
}
