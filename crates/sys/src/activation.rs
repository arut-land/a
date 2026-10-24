//! Opt-in service-manager ownership. Registration is an installation artifact,
//! never a daemon setting; an unavailable registered service must not be bypassed.

use std::io;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;

use crate::{Home, InstallLayout, Listener};

#[cfg(target_os = "linux")]
#[path = "activation/linux.rs"]
mod native;
#[cfg(target_os = "macos")]
#[path = "activation/macos.rs"]
mod native;
#[cfg(windows)]
#[path = "activation/windows.rs"]
mod native;

pub fn installed(home: &Home) -> io::Result<bool> {
    match std::fs::symlink_metadata(record(home)) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub(crate) fn record(home: &Home) -> PathBuf {
    home.run_dir().join("activation")
}

fn native_home(home: &Home) -> io::Result<()> {
    if home.layout() != &InstallLayout::Native {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "activation requires a native installation; A_HOME instances use self-spawn",
        ));
    }
    Ok(())
}

/// `app` prepares registration by the installed app's SMAppService UI.
pub fn install(home: &Home, app: bool) -> io::Result<()> {
    native_home(home)?;
    native::install(home, app)
}

pub fn uninstall(home: &Home, app: bool) -> io::Result<()> {
    native_home(home)?;
    native::uninstall(home, app)
}

/// Called before any runtime or child threads exist.
pub fn listener(home: &Home) -> io::Result<Listener> {
    native_home(home)?;
    native::listener(home)
}

/// A registered instance never silently becomes an unmanaged process, even
/// when the user disabled its background item in the OS settings.
pub fn require_unmanaged(home: &Home) -> io::Result<()> {
    if installed(home)? {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "native activation is installed but unavailable; check your user service manager or run `a daemon uninstall-activation`",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn run(command: &mut Command) -> io::Result<()> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn prepare(home: &Home, kind: &str) -> io::Result<()> {
    crate::fs::private_dir(&home.run_dir())?;
    crate::fs::write_private(&record(home), kind.as_bytes())
}

#[cfg(unix)]
fn forget(home: &Home) -> io::Result<()> {
    for path in [record(home), home.run_dir().join("daemon.endpoint")] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_instances_cannot_register_native_services() {
        let home = Home::from(std::env::temp_dir().join("a-activation-isolated"));
        assert_eq!(
            install(&home, false).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            uninstall(&home, false).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            listener(&home).err().unwrap().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
