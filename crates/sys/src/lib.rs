//! Platform specifics. The only crate allowed to use `unsafe`.
#![allow(unsafe_code)]

//!
//! Everything a platform does its own way sits behind a type here (a
//! process [`Group`], the daemon's [`Endpoint`], [`Signals`], the [`fs`]
//! operations), so no crate above names a platform: Unix is complete, and
//! on Windows what its daemon still lacks says it is unsupported.

mod awake;
pub mod config;
pub mod crash;
mod endpoint;
pub mod fs;
pub mod install;
pub mod log;
mod login;
mod proc;
#[cfg(unix)]
mod runtime;
#[cfg(not(unix))]
#[path = "unstaged.rs"]
mod runtime;
mod signals;
mod workspace;

use std::io;
use std::path::{Path, PathBuf};

pub use awake::Awake;
pub use config::Config;
pub use endpoint::{Endpoint, Listener, Stream};
pub use fs::{read_exact_at, sync_data};
pub use login::login_env;
pub use proc::{
    ExitWatch, Group, Pass, Passed, adopt_lock, boot_id, detach_session, exit_signal, exit_watch,
    pass, process_started, signal_process,
};
pub use runtime::{Runtime, Staged, VERSION};
pub use signals::{Signals, reraise};
pub use workspace::{GitEnv, workspace_in, workspace_of};

/// Locations under the per-user home (`$A_HOME`, default `~/.a`).
#[derive(Clone, Debug)]
pub struct Home {
    root: PathBuf,
}

impl From<PathBuf> for Home {
    fn from(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Home {
    pub fn from_env() -> io::Result<Self> {
        let root = match std::env::var_os("A_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => std::env::home_dir()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?
                .join(".a"),
        };
        Ok(root.into())
    }

    /// `$A_HOME`, else `~/.a`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// Locked by the running daemon for its whole life, and holds its pid.
    pub fn daemon_pid_file(&self) -> PathBuf {
        self.run_dir().join("daemon.pid")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Where tools keep output too large for a session's log, a private
    /// directory per session.
    pub fn spill_dir(&self) -> PathBuf {
        self.root.join("spill")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.logs_dir().join("daemon.log")
    }

    /// The daemon's crash reports; see [`crash`].
    pub fn crash_dir(&self) -> PathBuf {
        self.root.join("crash")
    }

    /// Content-addressed attachments too large for a log.
    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    /// A link to the daemon socket, where a person looking under the home
    /// finds it. Clients connect to [`Home::endpoint`] itself, whose path
    /// is short enough for `sun_path` wherever the home is.
    pub fn socket_link(&self) -> PathBuf {
        self.run_dir().join("daemon.sock")
    }

    /// The user's settings; see [`Config`].
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn runtime(&self) -> Runtime {
        Runtime::new(self.root.join("runtime"))
    }
}

/// What [`signal_process`] and [`Group::signal`] send, and [`Signals`]
/// catches. On Windows, console control events stand for the ones a
/// console has (Ctrl-C, Ctrl-Break, close).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM: exit cleanly.
    Terminate,
    /// SIGKILL.
    Kill,
    /// SIGINT: Ctrl-C.
    Interrupt,
    /// SIGHUP: the terminal went away.
    Hangup,
    /// SIGWINCH: the terminal changed size.
    Resize,
}

/// Stable across builds, unlike `std`'s hashers, so every version of `a`
/// finds the same endpoint and the same staging stamps.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}
