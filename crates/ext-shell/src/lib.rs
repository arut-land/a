//! One native shell tool and the session working directory it keeps.
//!
//! Startup selects Bash on Unix. Windows searches the daemon's captured
//! PATH, Git for Windows' user/machine registry install paths and conventional
//! Git/MSYS2 locations, excluding the Windows system WSL launcher. Without
//! native Bash it selects pwsh, else Windows PowerShell 5.1. Exactly one SDK
//! tool type is registered; neither a client PATH change nor a launch failure
//! changes that catalog. Restart the daemon to rediscover installed shells.
//!
//! Tool descriptions and prompt context share startup `ExecutionFacts`.
//! Commands retain the client's environment and cwd, with noninteractive
//! output preferences and the published temporary directory. Bash startup
//! hooks `BASH_ENV`/`ENV` are suppressed. Foreground cwd reports are removed after
//! use; truncated output is sealed into a blob the result references, and a
//! background job's output stays an open blob stream while it runs.
//!
//! PowerShell takes one bounded Unicode `-Command` payload, preserving normal
//! error semantics and explicit exits without changing execution policy. Its
//! plain-text host streams avoid Windows PowerShell's `EncodedCommand` CLIXML.
//! Native program output remains bytes, displayed with UTF-8 replacement.
//!
//! The extension provides [`CwdOf`] and [`Processes`]; process creation uses
//! the SDK process service and the platform's existing `SpawnPlan` backend.

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

pub use bash::{Bash, PowerShell, WindowsPowerShell};

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
        sys::shell::Dialect::WindowsPowerShell => {
            r.tool(WindowsPowerShell::new(facts));
        }
    }
    r.fold::<state::Cwd>()
            .provide::<dyn CwdOf>(Arc::new(Shell))
            .provide::<dyn Processes>(Arc::new(process::Provider))
            .text(Text::system("shell", 10, format!(
                "`{}` runs one command per call in a fresh shell. Filesystem working-directory changes persist; variables do not. No stdin or terminal: avoid prompts. Long output is cut to its start and end; the whole output is saved at the returned path. {} Set timeout (seconds, 120 by default, at most 600) for slow commands; background: true starts session-owned work and returns its log.",
                facts.tool(), match facts.dialect {
                    sys::shell::Dialect::Bash => "Search saved output with grep or sed -n.",
                    sys::shell::Dialect::PowerShell | sys::shell::Dialect::WindowsPowerShell => "Read saved output with Get-Content -Tail or Select-String -LiteralPath; use $env:NAME for environment variables.",
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
