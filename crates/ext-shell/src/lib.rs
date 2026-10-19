//! `bash`, and the session's working directory it keeps. The extension
//! provides the [`CwdOf`] and [`Spawner`] services: where commands run, and
//! how the processes tools start are started.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ext::{CwdOf, Cx, Extension, Registrar, SpawnSpec, Spawner, States, Text, Tool};
use protocol::Environment;

pub mod bash;
mod job;
mod output;
pub mod state;

pub use bash::Bash;

/// The `shell` extension.
pub struct Shell;

impl Extension for Shell {
    const NAME: &'static str = "shell";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Bash)
            .fold::<state::Cwd>()
            .provide::<dyn CwdOf>(Arc::new(Shell))
            .provide::<dyn Spawner>(Arc::new(Shell))
            .text(Text::system("bash", 10, "`bash` runs one command per call in a fresh shell whose working directory persists between calls (`cd` sticks; variables do not). There is no stdin or terminal: use flags that avoid prompts. Long output is cut to its start and end, with the whole saved to a file the output names: search it with `grep` or `sed -n` rather than reading it all. Set `timeout` (seconds, 120 by default, at most 600) for slow commands, and `background: true` for servers and watchers, then read their log.").for_tool(Bash::NAME));
    }
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

impl Spawner for Shell {
    fn process(&self, cx: &Cx, mut spawn: SpawnSpec) -> Result<std::process::Command, String> {
        cx.spawn_hooks(&mut spawn);
        if spawn.argv.is_empty() {
            return Err("a spawn hook left no program to run".to_owned());
        }
        Ok(bash::process(&spawn))
    }
}

#[cfg(test)]
mod tests;
