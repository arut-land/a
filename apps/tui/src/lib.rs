//! `a`'s terminal client: full screen on the alternate buffer with the
//! mouse, or (`A_TUI=inline`) a viewport under the shell's scrollback. Both
//! draw the shared reducer's view through one renderer; see `docs/tui.md`.
//! Its own crate so it builds for size, like the web UI: it draws at the
//! pace of one person's typing.
//!
//! One thread owns the app and the screen and blocks on a channel; the
//! terminal and the daemon each feed it from their own thread, so an idle
//! client never wakes.

mod app;
mod editor;
mod keys;
mod render;
#[cfg(unix)]
mod run;
#[cfg(unix)]
mod term;
mod text;
mod view;

use std::io::{self, IsTerminal};

use client::Endpoint;
use protocol::DaemonMsg;
use sys::Staged;

pub use render::Viewport;

/// The daemon to attach through, and this build (which a reconnect may
/// start).
#[derive(Clone)]
pub struct Daemon {
    pub endpoint: Endpoint,
    pub staged: Staged,
}

/// The client to run at this terminal: none (the line REPL) unless stdin
/// and stdout are both terminals; `A_TUI=plain` asks for lines anyway,
/// `A_TUI=inline` for the inline viewport.
pub fn viewport() -> Option<Viewport> {
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
enum Event {
    Input(keys::Input),
    Resized,
    /// stdin closed: the terminal is gone.
    InputClosed,
    /// Connection `id`'s writer, once it sent its attach; and whether the
    /// daemon is a remote host's.
    Connected(u64, client::Writer, bool),
    Daemon(u64, DaemonMsg),
    Closed(u64, String),
    /// Connection `id` could not be opened.
    Failed(u64, io::Error),
    Notice(String),
}

#[cfg(unix)]
pub use run::run;

#[cfg(not(unix))]
pub fn run(_daemon: &Daemon, _target: protocol::Target, _viewport: Viewport) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}
