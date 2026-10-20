//! `a`'s terminal client: full screen on the alternate buffer with the
//! mouse, or (`A_TUI=inline`) a viewport under the shell's scrollback. Both
//! draw the shared reducer's view into a cell grid of their own and write
//! what changed as ANSI; see `docs/tui.md`.
//! Its own crate so it builds for size, like the web UI: it draws at the
//! pace of one person's typing.
//!
//! One thread owns the app and the screen and blocks on a channel; the
//! terminal and the daemon each feed it from their own thread, so an idle
//! client never wakes.
//!
//! Unix only, like the daemon: elsewhere [`viewport`] picks the line REPL.

#[cfg(unix)]
mod app;
#[cfg(unix)]
mod editor;
#[cfg(unix)]
mod keys;
#[cfg(unix)]
mod render;
#[cfg(unix)]
mod run;
#[cfg(unix)]
mod screen;
#[cfg(unix)]
mod term;
#[cfg(unix)]
mod text;
/// Items as rows: public for the test that every renderer shows an item
/// the same way.
#[cfg(unix)]
#[doc(hidden)]
pub mod view;

use std::io::{self, IsTerminal};

/// Where frames go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewport {
    /// The whole alternate screen.
    Full,
    /// The last rows of the normal buffer, under the scrollback.
    Inline,
}

/// The client to run at this terminal: none (the line REPL) unless stdin
/// and stdout are both terminals; `A_TUI=plain` asks for lines anyway,
/// `A_TUI=inline` for the inline viewport.
pub fn viewport() -> Option<Viewport> {
    // The daemon is Unix only for now: elsewhere the REPL says so.
    if !cfg!(unix) || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    if std::env::var_os("TERM").is_some_and(|t| t == "dumb") {
        return None;
    }
    match std::env::var("A_TUI").as_deref() {
        Ok("plain" | "0" | "off") => None,
        Ok("inline") => Some(Viewport::Inline),
        _ => Some(Viewport::Full),
    }
}

/// What wakes the event loop.
#[cfg(unix)]
enum Event {
    Input(keys::Input),
    /// The terminal changed size.
    Resized,
    /// stdin closed: the terminal is gone.
    InputClosed,
    /// From the session client's driver.
    Client(client::Event),
    /// The working indicator's clock; only ever waited for while a turn
    /// runs.
    Tick,
}

#[cfg(unix)]
pub use run::run;

/// [`viewport`] never picks a client here.
#[cfg(not(unix))]
pub fn run(_: &client::Daemon, _: protocol::Target, _: Viewport) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}
