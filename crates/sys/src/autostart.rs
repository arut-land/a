//! Optional start at login. The per-user OS registration is the only state:
//! present means on. A client's self-spawn never reads it, so turning it on or
//! off cannot change how fast a first connect starts a daemon.

use std::io;
#[cfg(unix)]
use std::process::{Command, Stdio};

use crate::{Home, InstallLayout};

#[cfg(target_os = "linux")]
#[path = "autostart/linux.rs"]
mod native;
#[cfg(target_os = "macos")]
#[path = "autostart/macos.rs"]
mod native;
#[cfg(windows)]
#[path = "autostart/windows.rs"]
mod native;

/// How the login entry brings a daemon up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    /// Serves in the foreground the service manager tracks.
    Serve,
    /// Starts a detached daemon as a client does, and exits: the entry's
    /// host does not outlive the process it started.
    Spawn,
}

pub const LOGIN: Login = native::LOGIN;

fn native_home(home: &Home) -> io::Result<()> {
    if home.layout() != &InstallLayout::Native {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "autostart is for the native installation; A_HOME instances start on first use",
        ));
    }
    Ok(())
}

/// Whether this home can have autostart at all: only the native
/// installation registers, so an isolated daemon never asks.
pub fn applies(home: &Home) -> bool {
    native_home(home).is_ok()
}

/// Registers the daemon to start when this user logs in.
pub fn enable(home: &Home) -> io::Result<()> {
    native_home(home)?;
    // The registration names the installed build: install the running one.
    home.runtime().stage(&std::env::current_exe()?)?;
    native::enable(home)
}

/// Removes the registration; the running daemon is left alone.
pub fn disable(home: &Home) -> io::Result<()> {
    native_home(home)?;
    native::disable(home)
}

/// Run by a serving daemon off any reply's path: brings an existing
/// registration up to date with this build and layout, and says whether
/// autostart is on, so the daemon stays up instead of exiting when idle.
pub fn refresh(home: &Home) -> bool {
    native_home(home).is_ok() && native::refresh(home).unwrap_or(false)
}

/// The login entry: before any runtime or child thread exists, hand over to
/// the build the install selects, so an older registered entry point (the
/// app's bundled helper) never serves only to be drained by the first client.
pub fn enter(home: &Home, executing_build: &str) -> io::Result<()> {
    native_home(home)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let runtime = home.runtime();
        let running = std::env::current_exe()?;
        if let Some(executable) = handover(
            runtime.current().as_deref(),
            executing_build,
            &runtime.current_exe(),
            &running,
        ) {
            // Same PID: the service manager keeps tracking the daemon.
            return Err(Command::new(executable)
                .args(std::env::args_os().skip(1))
                .exec());
        }
    }
    #[cfg(windows)]
    let _ = executing_build;
    Ok(())
}

/// The executable to hand over to, if any. Never the one running: an image
/// whose id differs from the marker naming it would otherwise exec itself
/// forever.
#[cfg(unix)]
fn handover(
    selected: Option<&str>,
    executing_build: &str,
    current_exe: &std::path::Path,
    running: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let same = |a: &std::path::Path, b: &std::path::Path| matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b);
    (selected.is_some_and(|selected| selected != executing_build)
        && current_exe.is_file()
        && !same(current_exe, running))
    .then(|| current_exe.to_owned())
}

#[cfg(unix)]
fn run(command: &mut Command) -> io::Result<()> {
    let output = command.stdin(Stdio::null()).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Writes `contents` to `path` unless it already holds them; says whether it wrote.
#[cfg(unix)]
fn update(path: &std::path::Path, contents: &[u8]) -> io::Result<bool> {
    if std::fs::read(path).is_ok_and(|current| current == contents) {
        return Ok(false);
    }
    // Creates a missing parent with ordinary permissions: the service
    // manager's directory is the user's, not ours to make private.
    crate::fs::write_private(path, contents)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_login_entry_hands_over_at_most_once() {
        let root =
            std::env::temp_dir().join(format!("a-autostart-handover-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let (bundled, selected) = (root.join("bundled"), root.join("selected"));
        std::fs::write(&bundled, "old").unwrap();
        std::fs::write(&selected, "new").unwrap();
        assert_eq!(
            handover(Some("new"), "old", &selected, &bundled),
            Some(selected.clone())
        );
        // The selected image reports another id than its marker names: it
        // is already running, so it serves rather than exec itself again.
        assert_eq!(handover(Some("new"), "other", &selected, &selected), None);
        assert_eq!(handover(Some("old"), "old", &selected, &bundled), None);
        assert_eq!(handover(None, "old", &selected, &bundled), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn isolated_instances_cannot_register_at_login() {
        let home = Home::from(std::env::temp_dir().join("a-autostart-isolated"));
        assert_eq!(
            enable(&home).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            disable(&home).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert!(!refresh(&home));
        assert_eq!(
            enter(&home, "test").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
