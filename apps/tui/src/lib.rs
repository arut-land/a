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
//! Where raw terminal input is unsupported, [`viewport`] picks the line REPL.

mod app;
mod editor;
mod keys;
mod render;
mod run;
mod screen;
mod term;
mod text;
/// Items as rows: public for the test that every renderer shows an item
/// the same way.
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

/// `A_TUI=plain` (or `0`, `off`): the line REPL, wherever `a` runs.
pub fn plain() -> bool {
    matches!(std::env::var("A_TUI").as_deref(), Ok("plain" | "0" | "off"))
}

/// The client to run at this terminal: none (the line REPL) unless stdin
/// and stdout are both terminals; [`plain`] asks for lines anyway,
/// `A_TUI=inline` for the inline viewport.
pub fn viewport() -> Option<Viewport> {
    // Raw input support is a platform capability, not a renderer decision.
    if !sys::terminal::SUPPORTED || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    if plain() || std::env::var_os("TERM").is_some_and(|t| t == "dumb") {
        return None;
    }
    match std::env::var("A_TUI").as_deref() {
        Ok("inline") => Some(Viewport::Inline),
        _ => Some(Viewport::Full),
    }
}

/// What wakes the event loop.
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

pub use run::run;
