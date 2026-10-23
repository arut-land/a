//! `bash`, and the session's working directory it keeps. The extension
//! provides the [`CwdOf`] and [`Processes`] services: where commands run, and
//! how the processes tools start are started.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ext::{
    Configured, CwdOf, Extension, Processes, Provides, Refused, Registrar, States, Table, Text,
};
use protocol::Environment;

pub mod bash;
mod dialect;
mod output;
mod process;
pub mod state;

pub use bash::{Bash, PowerShell};

/// The `shell` extension.
pub struct Shell;

impl Provides<dyn CwdOf> for Shell {}
impl Provides<dyn Processes> for Shell {}

impl Configured for Shell {
    fn configure(_: &impl Table) -> Result<Self, Refused> {
        Ok(Self)
    }
}

impl Extension for Shell {
    const NAME: &'static str = "shell";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        contribute(r, sys::shell::ExecutionFacts::current());
    }
}

fn contribute(r: &mut Registrar, facts: &'static sys::shell::ExecutionFacts) {
    match facts.dialect {
        sys::shell::Dialect::Bash => {
            r.tool(Bash::new(facts));
        }
        sys::shell::Dialect::PowerShell => {
            r.tool(PowerShell::new(facts));
        }
    }
    r.fold::<state::Cwd>()
            .provide::<dyn CwdOf>(Arc::new(Shell))
            .provide::<dyn Processes>(Arc::new(process::Provider))
            .text(Text::system("shell", 10, format!(
                "`{}` runs one command per call in a fresh shell. Filesystem working-directory changes persist; variables do not. No stdin or terminal: avoid prompts. Long output is cut to its start and end; the whole output is saved at the returned path. {} Set timeout (seconds, 120 by default, at most 600) for slow commands; background: true starts session-owned work and returns its log.",
                facts.tool(), match facts.dialect {
                    sys::shell::Dialect::Bash => "Search saved output with grep or sed -n.",
                    sys::shell::Dialect::PowerShell => "Read saved output with Get-Content -Tail or Select-String -LiteralPath; use $env:NAME for environment variables.",
                }
            )).for_tool(facts.tool()));
}

/// Where the session's last command left off, unless the client has since
/// moved, else the client's directory, else the workspace. One that was
/// removed since is skipped.
impl CwdOf for Shell {
    fn dir(&self, workspace: &Path, env: Option<&Environment>, states: &States) -> PathBuf {
        let base = env.map(|env| env.cwd.as_path());
        let last = states.get::<state::Cwd>().and_then(|cwd| cwd.after(base));
        last.into_iter()
            .chain(base)
            .find(|dir| dir.is_dir())
            .unwrap_or(workspace)
            .to_path_buf()
    }
}

#[cfg(test)]
mod tests;
