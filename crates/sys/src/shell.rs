//! Startup shell selection and the execution contract shared by tools and prompts.

use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(unix)]
#[path = "shell/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "shell/windows.rs"]
mod imp;

pub use imp::{find_bash, validate_argv};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Bash,
    PowerShell,
    WindowsPowerShell,
}

/// One immutable choice for the daemon's lifetime. Client environments do
/// not rename an already published tool or redirect its executable.
#[derive(Debug)]
pub struct ExecutionFacts {
    pub dialect: Dialect,
    pub executable: PathBuf,
    pub syntax: &'static str,
    pub paths: &'static str,
    pub utilities: &'static str,
    pub cleanup: &'static str,
    pub temp: PathBuf,
    pub cwd_command: &'static str,
    pub runtime_bin: Option<PathBuf>,
}

static FACTS: OnceLock<ExecutionFacts> = OnceLock::new();

impl ExecutionFacts {
    /// The facts the daemon published; using them before
    /// [`initialize`](Self::initialize) is a startup-order bug, never a
    /// reason to resolve a different layout from the environment.
    pub fn current() -> &'static Self {
        #[cfg(any(test, feature = "testing"))]
        if FACTS.get().is_none() {
            // One fixed root, so test processes leave nothing behind per run.
            let root = std::env::temp_dir().join("a-test-shell");
            Self::initialize(&crate::Paths::from(root)).expect("prepare test shell facts");
        }
        FACTS
            .get()
            .expect("the daemon initializes shell facts before extensions register")
    }

    /// Called by the daemon before registering extensions. The facts live
    /// for the process, so a second layout is refused rather than ignored;
    /// test processes that load several homes keep the first.
    pub fn initialize(paths: &crate::Paths) -> std::io::Result<&'static Self> {
        if FACTS.get().is_none() {
            let _ = FACTS.set(Self::resolve(paths)?);
        }
        let facts = FACTS.get().expect("initialized shell facts");
        if !cfg!(feature = "testing") && facts.temp != paths.temporary_dir() {
            return Err(std::io::Error::other(format!(
                "shell facts already use {}, not {}",
                facts.temp.display(),
                paths.temporary_dir().display()
            )));
        }
        Ok(facts)
    }

    fn resolve(paths: &crate::Paths) -> std::io::Result<Self> {
        Ok(imp::discover(paths.prepare_temporary_dir()?))
    }

    /// Keep the published temporary-directory contract independent of a
    /// later attaching client's environment.
    pub fn apply_environment(&self, env: &mut std::collections::BTreeMap<String, String>) {
        imp::apply_environment(self, env);
    }

    pub const fn tool(&self) -> &'static str {
        match self.dialect {
            Dialect::Bash => "bash",
            Dialect::PowerShell | Dialect::WindowsPowerShell => "powershell",
        }
    }

    pub fn context(&self) -> String {
        format!(
            "shell: {} ({})\ndialect: {}\npath style: {}\nutilities: {}\ntemp directory: {}\ncleanup: {}",
            self.tool(),
            self.executable.display(),
            self.syntax,
            self.paths,
            self.utilities,
            self.temp.display(),
            self.cleanup,
        )
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::process::Command;

    #[test]
    fn daemon_and_tool_children_keep_temporary_files_inside_the_isolated_root() {
        const STAGE: &str = "A_TEST_TEMP_STAGE";
        const ROOT: &str = "A_TEST_TEMP_ROOT";
        let child = || {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "shell::tests::daemon_and_tool_children_keep_temporary_files_inside_the_isolated_root",
            ]);
            command
        };
        match std::env::var(STAGE).ok().as_deref() {
            Some("tool") => {
                let expected = PathBuf::from(std::env::var_os(ROOT).unwrap()).join("tmp");
                assert_eq!(PathBuf::from(std::env::var_os("TMPDIR").unwrap()), expected);
                #[cfg(windows)]
                for name in ["TMP", "TEMP"] {
                    assert_eq!(PathBuf::from(std::env::var_os(name).unwrap()), expected);
                }
                assert_eq!(std::env::temp_dir(), expected);
                std::fs::write(std::env::temp_dir().join("child-created"), b"private").unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(
                        std::fs::metadata(expected).unwrap().permissions().mode() & 0o777,
                        0o700
                    );
                }
                crate::child::mark("tool");
            }
            Some("daemon") => {
                let paths = crate::Paths::from_env().unwrap();
                assert_eq!(std::env::temp_dir(), paths.temporary_dir());
                let facts = ExecutionFacts::initialize(&paths).unwrap();
                let mut env = BTreeMap::from([
                    ("TMPDIR".into(), "/outside".into()),
                    ("TMP".into(), "/outside".into()),
                    ("TEMP".into(), "/outside".into()),
                ]);
                facts.apply_environment(&mut env);
                let output = child()
                    .env_clear()
                    .envs(env)
                    .env(STAGE, "tool")
                    .env(ROOT, paths.root())
                    .output()
                    .unwrap();
                assert!(crate::child::ran(&output, "tool"), "{output:?}");
                assert!(paths.temporary_dir().join("child-created").is_file());
                crate::child::mark("daemon");
            }
            _ => {
                let root = std::env::temp_dir().join(format!(
                    "a-temp-child-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                let paths = crate::Paths::from(root.clone());
                let mut command = child();
                paths.daemon_env(&mut command);
                let output = command.env(STAGE, "daemon").output().unwrap();
                std::fs::remove_dir_all(root).unwrap();
                assert!(crate::child::ran(&output, "daemon"), "{output:?}");
            }
        }
    }
}
