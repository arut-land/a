//! Platform specifics. The only crate allowed to use `unsafe`.
#![allow(unsafe_code)]
#![cfg_attr(
    windows,
    feature(windows_process_extensions_raw_attribute, windows_by_handle)
)]

//!
//! Everything a platform does its own way sits behind a type here (a
//! process [`RecordedIdentity`], the daemon's [`Endpoint`], [`Signals`], the [`fs`]
//! operations), so no crate above names a platform. Local daemon transports are Unix
//! sockets and Windows named pipes with per-user security.

mod awake;
pub mod config;
pub mod crash;
mod desktop;
mod endpoint;
pub mod fs;
pub mod install;
pub mod log;
mod login;
mod memory;
mod paths;
mod platform;
mod proc;
mod runtime;
pub mod shell;
mod signals;
pub mod terminal;
#[cfg(windows)]
mod windows;
#[cfg(any(test, windows))]
mod windows_quote;
mod workspace;

use std::path::{Path, PathBuf};

pub use awake::Awake;
pub use config::Config;
pub use desktop::open_browser;
pub use endpoint::{Endpoint, EndpointRetirement, Listener, OwnedConnection, Stream};
pub use fs::{read_exact_at, sync_data, write_watch};
pub use login::login_env;
pub use memory::small_footprint;
/// The resolved locations belonging to one daemon.
pub use paths::Paths as Home;
pub use paths::{InstallLayout, Paths, RemoteLayout};
pub use platform::{Arch, Os, Platform};
pub use proc::{
    AdoptLockError, Child, ContainedChild, DaemonLock, ExitWatch, InheritedNumber, OutputCapture,
    Pass, Passed, ProcessLease, ProcessPipe, RecordedIdentity, RecordedProcess, adopt_lock,
    boot_id, daemon_ready, detach_session, exit_signal, find_bash, process_started, spawn_daemon,
    spawn_group, spawn_group_captured, spawn_ssh, spawn_stdio,
};
pub use runtime::{Runtime, Staged, VERSION};
pub use signals::{Signals, reraise};
#[cfg(windows)]
pub use windows::CompletionSource;
pub use workspace::{GitEnv, workspace_in, workspace_of};

/// What [`ProcessLease::signal`] send, and [`Signals`]
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

/// A native path as understood by the shell executing a session's tools.
pub fn shell_path(path: &Path) -> std::borrow::Cow<'_, str> {
    #[cfg(unix)]
    {
        path.to_string_lossy()
    }
    #[cfg(windows)]
    {
        let path = path.to_string_lossy();
        if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
            format!("//{}", unc.replace('\\', "/")).into()
        } else {
            path.trim_start_matches(r"\\?\").replace('\\', "/").into()
        }
    }
}

/// The directory reported by that shell, as a native absolute path.
pub fn shell_directory(text: &str) -> PathBuf {
    let text = text.trim_end_matches(['\r', '\n']);
    #[cfg(windows)]
    {
        let bytes = text.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b'/'
        {
            let path = PathBuf::from(format!("{}:{}", char::from(bytes[1]), &text[2..]));
            return path.canonicalize().unwrap_or(path);
        }
    }
    let path = PathBuf::from(text);
    #[cfg(windows)]
    return path.canonicalize().unwrap_or(path);
    #[cfg(unix)]
    path
}

/// The shell builtin that reports a directory in the host's path syntax.
pub fn shell_pwd() -> &'static str {
    #[cfg(unix)]
    {
        "pwd -P"
    }
    #[cfg(windows)]
    {
        shell::ExecutionFacts::current().cwd_command
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn shell_directory_reports_roundtrip_spaces_unicode_and_line_endings() {
        let path = std::env::temp_dir().join("space café");
        let report = format!("{}\r\n", shell_path(&path));
        assert_eq!(shell_directory(&report), path);
    }

    #[cfg(windows)]
    #[test]
    fn extended_and_unc_paths_keep_their_shell_meaning() {
        assert_eq!(shell_path(Path::new(r"\\?\C:\space café")), "C:/space café");
        assert_eq!(
            shell_path(Path::new(r"\\?\UNC\server\share\dir")),
            "//server/share/dir"
        );
        assert_eq!(
            shell_directory("/c/space café\n"),
            PathBuf::from("c:/space café")
        );
    }
}
