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
    if !native::SUPPORTED {
        return Ok(false);
    }
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

/// Serializes registration changes with clients trying to self-spawn.
pub struct Installation<'a> {
    home: &'a Home,
    _lock: std::fs::File,
}

impl<'a> Installation<'a> {
    pub fn claim(home: &'a Home) -> io::Result<Self> {
        native_home(home)?;
        if !native::SUPPORTED {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this platform uses per-user self-spawn",
            ));
        }
        crate::fs::private_dir(&home.run_dir())?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(home.run_dir().join("start.lock"))?;
        file.lock()?;
        Ok(Self { home, _lock: file })
    }

    /// `app` prepares registration by the installed app's `SMAppService` UI.
    pub fn install(&self, app: bool, stop: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        native::install(self.home, app, stop)
    }

    pub fn uninstall(&self, app: bool) -> io::Result<()> {
        native::uninstall(self.home, app)
    }
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
    crate::endpoint::register_activation(home, kind)
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
            Installation::claim(&home).err().unwrap().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            listener(&home).err().unwrap().kind(),
            io::ErrorKind::Unsupported
        );
    }
}
