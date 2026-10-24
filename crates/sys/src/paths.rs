//! Native directory purposes, resolved once at the client/daemon boundary.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::Runtime;

mod remote;
pub use remote::RemoteLayout;

#[cfg(target_os = "linux")]
#[path = "paths/linux.rs"]
mod native;
#[cfg(target_os = "macos")]
#[path = "paths/macos.rs"]
mod native;
#[cfg(windows)]
#[path = "paths/windows.rs"]
mod native;

/// Native per-user locations or an explicit, self-contained instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallLayout {
    Native,
    Isolated(PathBuf),
}

/// All locations for one daemon. Resolution does not create directories or
/// require a writable configuration/home directory. Clones share the resolved
/// locations across clients and session handles.
#[derive(Clone, Debug)]
pub struct Paths {
    inner: Arc<Locations>,
}

#[derive(Debug)]
struct Locations {
    layout: InstallLayout,
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
    cache: PathBuf,
    logs: PathBuf,
    temporary: PathBuf,
    runtime: PathBuf,
}

impl From<PathBuf> for Paths {
    fn from(root: PathBuf) -> Self {
        Self::new(Locations {
            layout: InstallLayout::Isolated(root.clone()),
            config: root.clone(),
            data: root.clone(),
            state: root.clone(),
            cache: root.join("cache"),
            logs: root.join("logs"),
            temporary: root.join("tmp"),
            runtime: root.join("run"),
        })
    }
}

impl Paths {
    fn new(locations: Locations) -> Self {
        Self {
            inner: Arc::new(locations),
        }
    }

    pub fn from_env() -> io::Result<Self> {
        match std::env::var_os("A_HOME") {
            Some(root) => {
                if root.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "A_HOME is empty",
                    ));
                }
                // Resolve before a daemon changes cwd; relative overrides must
                // never select a different instance in the child.
                Ok(std::path::absolute(root)?.into())
            }
            None => native::resolve(),
        }
    }

    pub fn layout(&self) -> &InstallLayout {
        &self.inner.layout
    }

    /// The durable instance identity, also used for endpoint ownership.
    pub fn root(&self) -> &Path {
        &self.inner.state
    }

    pub fn config_dir(&self) -> &Path {
        &self.inner.config
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data
    }

    pub fn state_dir(&self) -> &Path {
        &self.inner.state
    }

    pub fn cache_dir(&self) -> &Path {
        &self.inner.cache
    }

    pub fn temporary_dir(&self) -> &Path {
        &self.inner.temporary
    }

    /// IPC, never executable staging or retained output.
    pub fn runtime_dir(&self) -> &Path {
        &self.inner.runtime
    }

    /// Preserve directory purposes across the daemon's cleared environment.
    pub fn daemon_env(&self, command: &mut Command) {
        match &self.inner.layout {
            InstallLayout::Isolated(root) => {
                command.env("A_HOME", root);
            }
            InstallLayout::Native => {
                command.env_remove("A_HOME");
                native::daemon_env(self, command);
            }
        }
    }
    pub fn run_dir(&self) -> PathBuf {
        self.inner.state.join("run")
    }

    /// Locked by the running daemon for its whole life, and holds its pid.
    pub fn daemon_pid_file(&self) -> PathBuf {
        self.run_dir().join("daemon.pid")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.inner.state.join("sessions")
    }

    /// How every session lists, derived from their logs by the daemon.
    pub fn catalog_file(&self) -> PathBuf {
        self.inner.state.join("sessions.idx")
    }

    /// Where tools keep output too large for a session's log, a private
    /// directory per session.
    pub fn spill_dir(&self) -> PathBuf {
        self.inner.state.join("spill")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.inner.logs.clone()
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.logs_dir().join("daemon.jsonl")
    }

    /// The daemon's crash reports; see [`crate::crash`].
    pub fn crash_dir(&self) -> PathBuf {
        match self.layout() {
            InstallLayout::Native => self.inner.logs.join("crash"),
            InstallLayout::Isolated(root) => root.join("crash"),
        }
    }

    /// Content-addressed attachments too large for a log.
    pub fn blobs_dir(&self) -> PathBuf {
        self.inner.state.join("blobs")
    }

    /// A link to the daemon socket, where a person looking under the home
    /// finds it. Clients connect to [`Paths::endpoint`] itself, whose path
    /// is short enough for `sun_path` wherever the home is.
    pub fn socket_link(&self) -> PathBuf {
        self.run_dir().join("daemon.sock")
    }

    /// The user's settings; see [`crate::Config`].
    pub fn config_file(&self) -> PathBuf {
        self.inner.config.join("config.toml")
    }

    /// The operator's permission rules, which only tighten; see
    /// [`crate::config::rules`].
    pub fn policy_file(&self) -> PathBuf {
        self.inner.config.join("policy.toml")
    }

    pub fn runtime(&self) -> Runtime {
        Runtime::new(self.inner.data.join("runtime"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_isolated_home_never_resolves_storage_outside_its_root() {
        let root = std::env::temp_dir().join("a-paths-isolated");
        let paths = Paths::from(root.clone());
        for path in [
            paths.config_dir().to_owned(),
            paths.data_dir().to_owned(),
            paths.state_dir().to_owned(),
            paths.cache_dir().to_owned(),
            paths.temporary_dir().to_owned(),
            paths.runtime_dir().to_owned(),
            paths.sessions_dir(),
            paths.catalog_file(),
            paths.spill_dir(),
            paths.logs_dir(),
            paths.daemon_log(),
            paths.crash_dir(),
            paths.blobs_dir(),
            paths.run_dir(),
            paths.daemon_pid_file(),
            paths.socket_link(),
            paths.config_file(),
            paths.policy_file(),
            paths.runtime().dir().to_owned(),
        ] {
            assert!(
                path.starts_with(&root),
                "{} escapes {}",
                path.display(),
                root.display()
            );
        }
    }

    #[cfg(all(unix, not(miri)))]
    #[test]
    fn remote_bootstrap_and_native_resolution_choose_the_same_build_directory() {
        use std::os::unix::ffi::OsStrExt;
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{}\nprintf '%s' \"$a_runtime\"",
                RemoteLayout::for_platform(crate::Platform::CURRENT).runtime_script()
            ))
            .env_remove("A_HOME")
            .output()
            .unwrap();
        assert!(output.status.success());
        let native = native::resolve().unwrap();
        assert_eq!(output.stdout, native.runtime().dir().as_os_str().as_bytes());
    }

    #[cfg(all(unix, not(miri)))]
    #[test]
    fn remote_bootstrap_honors_isolation_and_refuses_an_empty_override() {
        use std::os::unix::ffi::OsStrExt;
        let root = std::env::temp_dir().join("a paths café");
        let script = format!(
            "{}\nprintf '%s' \"$a_runtime\"",
            RemoteLayout::for_platform(crate::Platform::CURRENT).runtime_script()
        );
        let output = Command::new("sh")
            .args(["-c", &script])
            .env("A_HOME", &root)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, root.join("runtime").as_os_str().as_bytes());
        assert!(
            !Command::new("sh")
                .args(["-c", &script])
                .env("A_HOME", "")
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn a_spawned_daemon_keeps_the_explicit_isolated_layout() {
        let paths = Paths::from(std::env::temp_dir().join("a-paths-child"));
        let mut command = Command::new("a");
        paths.daemon_env(&mut command);
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "A_HOME")
                .unwrap()
                .1,
            Some(paths.root().as_os_str())
        );
    }
}
