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

fn native_home(home: &Home) -> io::Result<()> {
    if home.layout() != &InstallLayout::Native {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "autostart is for the native installation; A_HOME instances start on first use",
        ));
    }
    Ok(())
}

/// Registers the daemon to start when this user logs in.
pub fn enable(home: &Home) -> io::Result<()> {
    native_home(home)?;
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
        if runtime
            .current()
            .is_some_and(|selected| selected != executing_build)
        {
            let executable = runtime.current_exe();
            if executable.is_file() {
                // Same PID: the service manager keeps tracking the daemon.
                return Err(Command::new(executable)
                    .args(std::env::args_os().skip(1))
                    .exec());
            }
        }
    }
    #[cfg(windows)]
    let _ = executing_build;
    Ok(())
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
