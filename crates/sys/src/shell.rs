//! Startup shell selection and the execution contract shared by tools and prompts.

use std::path::PathBuf;
use std::sync::OnceLock;

#[cfg(unix)]
#[path = "shell/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "shell/windows.rs"]
mod imp;

pub use imp::find_bash;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Bash,
    PowerShell,
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
}

impl ExecutionFacts {
    pub fn current() -> &'static Self {
        static FACTS: OnceLock<ExecutionFacts> = OnceLock::new();
        FACTS.get_or_init(imp::discover)
    }

    pub const fn tool(&self) -> &'static str {
        match self.dialect {
            Dialect::Bash => "bash",
            Dialect::PowerShell => "powershell",
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
