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
//! use; truncated output is sealed into a blob the result references. A
//! command still running at its timeout goes on in the background, as a
//! forced steer would send it, and its result comes back on the queue. A
//! process group the session owns (a background job, or what a command left
//! running) writes an open blob stream, sealed as the group ends into the blob
//! the end's queued notice holds.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shell {
    /// Seconds a command is waited for, when its call does not say,
    /// before it moves to the background.
    pub timeout: u64,
}

impl Default for Shell {
    fn default() -> Self {
        Self {
            timeout: bash::TIMEOUT,
        }
    }
}

impl Provides<dyn CwdOf> for Shell {}
impl Provides<dyn Processes> for Shell {}

/// `[ext.shell] timeout`, in seconds, from 1 to [`bash::MAX_TIMEOUT`].
impl Configured for Shell {
    fn configure(table: &impl Table) -> Result<Self, Refused> {
        let Some(timeout) = table.whole("timeout")? else {
            return Ok(Self::default());
        };
        if !(1..=bash::MAX_TIMEOUT).contains(&timeout) {
            return Err(Refused(format!(
                "ext.shell.timeout is {timeout}: it takes 1 to {} seconds",
                bash::MAX_TIMEOUT
            )));
        }
        Ok(Self { timeout })
    }
}

impl Extension for Shell {
    const NAME: &'static str = "shell";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        contribute(r, sys::shell::ExecutionFacts::current(), self.timeout);
    }
}

fn contribute(r: &mut Registrar, facts: &'static sys::shell::ExecutionFacts, timeout: u64) {
    match facts.dialect {
        sys::shell::Dialect::Bash => {
            r.tool(Bash::new(facts, timeout));
        }
        sys::shell::Dialect::PowerShell => {
            r.tool(PowerShell::new(facts, timeout));
        }
        sys::shell::Dialect::WindowsPowerShell => {
            r.tool(WindowsPowerShell::new(facts, timeout));
        }
    }
    r.fold::<state::Cwd>()
            .provide::<dyn CwdOf>(Arc::new(Shell { timeout }))
            .provide::<dyn Processes>(Arc::new(process::Provider))
            .text(Text::system("shell", 10, format!(
                "`{}` runs one command per call in a fresh shell. Filesystem working-directory changes persist; variables do not. No stdin or terminal: avoid prompts. Long output is cut to its start and end; the whole output is saved at the returned path. {} A command still running after timeout seconds ({timeout} by default, at most {}) moves to the background rather than stopping: the call answers at once with where its output grows, and its result arrives later as a background-result. background: true starts session-owned work, such as a server or watcher, and returns at once; its end arrives with the next model call.",
                facts.tool(), match facts.dialect {
                    sys::shell::Dialect::Bash => "Search saved output with grep or sed -n.",
                    sys::shell::Dialect::PowerShell | sys::shell::Dialect::WindowsPowerShell => "Read saved output with Get-Content -Tail or Select-String -LiteralPath; use $env:NAME for environment variables.",
                },
                bash::MAX_TIMEOUT,
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
