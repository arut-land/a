//! The built-in extensions. Each registers through the same verbs as any
//! other, can be replaced by one of the same name, and is a cargo feature,
//! so leaving one out of a build is leaving it out of the registry.

#[cfg(feature = "prompt")]
use std::fmt::Write as _;
#[cfg(feature = "prompt")]
use std::io::Read;
#[cfg(feature = "shell")]
use std::path::{Path, PathBuf};
#[cfg(feature = "shell")]
use std::sync::Arc;

#[cfg(feature = "shell")]
use protocol::Environment;

#[cfg(test)]
use ext::DynTool;
#[cfg(feature = "prompt")]
use ext::Facts;
#[cfg(any(test, feature = "fixtures"))]
use ext::Replay;
#[cfg(any(feature = "prompt", feature = "shell"))]
use ext::Text;
#[cfg(feature = "shell")]
use ext::Tool;
use ext::host::Registry;
#[cfg(feature = "shell")]
use ext::{Cancel, CwdOf, Cx, Kill, SpawnSpec as Spawn, Spawner, States};
use ext::{DynExtension, Refused, Tier};
#[cfg(any(
    feature = "policy",
    feature = "prompt",
    feature = "shell",
    feature = "chat"
))]
use ext::{Extension, Registrar};

pub mod agents;
#[cfg(feature = "shell")]
pub mod state;
#[cfg(test)]
mod testing;
#[cfg(feature = "shell")]
pub mod tools;

/// The built-ins this build has, in their install order, with `policy`
/// as configured and guarded.
pub fn extensions(policy: &Policy) -> Vec<(Tier, Box<dyn DynExtension>)> {
    #[cfg(not(feature = "policy"))]
    let _ = policy;
    vec![
        #[cfg(feature = "policy")]
        (Tier::Guarded, Box::new(policy.clone())),
        #[cfg(feature = "prompt")]
        (Tier::Pre, Box::new(Prompt)),
        #[cfg(feature = "shell")]
        (Tier::Pre, Box::new(Shell)),
        #[cfg(feature = "edit")]
        (Tier::Pre, Box::new(Edit)),
        #[cfg(feature = "chat")]
        (Tier::Pre, Box::new(Chat)),
    ]
}

/// A registry of the built-ins: see [`extensions`].
pub fn builtins(policy: &Policy) -> Result<Registry, Refused> {
    extensions(policy)
        .iter()
        .try_fold(Registry::default(), |r, (tier, ext)| {
            r.with_dyn(*tier, &**ext)
        })
}

/// What a daemon runs with: the built-ins, and for the `echo` agent the
/// test fixtures, in a build that has them.
pub fn registry(policy: &Policy, agent: Option<&str>) -> Result<Registry, Refused> {
    let registry = builtins(policy)?;
    #[cfg(any(test, feature = "fixtures"))]
    if agent == Some(agents::ECHO) {
        return registry.with(&Fixtures);
    }
    let _ = agent;
    Ok(registry)
}

/// Which calls wait for the user's approval.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub ask: Vec<String>,
}

impl Policy {
    pub fn ask(tools: &[&str]) -> Self {
        Self {
            ask: tools.iter().map(|&t| t.to_owned()).collect(),
        }
    }
}

#[cfg(feature = "policy")]
impl Extension for Policy {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let ask = self.ask.clone();
        r.on::<ext::point::Permission>(move |_, call| {
            if ask.iter().any(|t| t == call.tool) {
                protocol::Permission::Ask
            } else {
                protocol::Permission::Allow
            }
        });
    }
}

/// The system prompt's one line of identity, and the facts the model is
/// first given about where it runs: the directory, the platform, and the
/// workspace's `AGENTS.md`.
#[cfg(feature = "prompt")]
pub struct Prompt;

/// The most of `AGENTS.md` the model is given.
#[cfg(feature = "prompt")]
const AGENTS_LIMIT: u64 = 32 * 1024;

#[cfg(feature = "prompt")]
impl Extension for Prompt {
    const NAME: &'static str = "prompt";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.text(Text::system("identity", 0, "You are `a`, a coding agent working in the user's workspace through the tools you are given. Do the work rather than describe it, and keep replies short."))
            .text(Text::context("environment", 0, |facts| Some(context(facts))));
    }
}

#[cfg(feature = "prompt")]
fn context(facts: &Facts<'_>) -> String {
    let mut text = format!(
        "<environment>\nworking directory: {}\n",
        facts.dir.display()
    );
    if facts.workspace != facts.dir {
        let _ = writeln!(text, "workspace: {}", facts.workspace.display());
    }
    let _ = write!(
        text,
        "platform: {} {}\n</environment>",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let path = facts.workspace.join("AGENTS.md");
    let mut notes = String::new();
    if std::fs::File::open(&path)
        .and_then(|file| file.take(AGENTS_LIMIT).read_to_string(&mut notes))
        .is_ok()
        && !notes.trim().is_empty()
    {
        let _ = write!(
            text,
            "\n\n<agents_md path=\"{}\">\n{}\n</agents_md>",
            path.display(),
            notes.trim()
        );
    }
    text
}

/// `bash`, and the session's working directory it keeps. It provides the
/// [`CwdOf`] and [`Spawner`] services: where commands run, and how the
/// processes tools start are started.
#[cfg(feature = "shell")]
pub struct Shell;

#[cfg(feature = "shell")]
impl Extension for Shell {
    const NAME: &'static str = "shell";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tools::Bash)
            .fold::<state::Cwd>()
            .provide::<dyn CwdOf>(Arc::new(Shell))
            .provide::<dyn Spawner>(Arc::new(Shell))
            .text(Text::system("bash", 10, "`bash` runs one command per call in a fresh shell whose working directory persists between calls (`cd` sticks; variables do not). There is no stdin or terminal: use flags that avoid prompts. Long output is cut to its start and end, with the whole saved to a file the output names: search it with `grep` or `sed -n` rather than reading it all. Set `timeout` (seconds, 120 by default, at most 600) for slow commands, and `background: true` for servers and watchers, then read their log.").for_tool(tools::Bash::NAME));
    }
}

/// Where the session's last command left off, unless the client has since
/// moved, else the client's directory, else the workspace. One that was
/// removed since is skipped.
#[cfg(feature = "shell")]
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

#[cfg(feature = "shell")]
impl Spawner for Shell {
    fn process(&self, cx: &Cx, mut spawn: Spawn) -> Result<std::process::Command, String> {
        cx.spawn_hooks(&mut spawn);
        if spawn.argv.is_empty() {
            return Err("a spawn hook left no program to run".to_owned());
        }
        Ok(tools::shell::process(&spawn))
    }
}

/// `edit`, and the hashes it keeps to catch stale reads. It edits where
/// commands run, so it needs [`CwdOf`].
#[cfg(feature = "edit")]
pub struct Edit;

#[cfg(feature = "edit")]
impl Extension for Edit {
    const NAME: &'static str = "edit";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tools::Edit)
            .fold::<tools::edit::Seen>()
            .need::<dyn CwdOf>()
            .text(Text::system("edit", 20, "`edit` changes a file by exact replacement: each `old` must appear exactly once in the file as it was before the call, so include enough of the text around it, and the edits of one call must not overlap. One edit with an empty `old` writes the whole file. Look at a file before editing it: an edit to a file that changed since this session last edited it fails until you look again.").for_tool(tools::Edit::NAME));
    }
}

/// The `chat` agent: see [`agents::Chat`].
#[cfg(feature = "chat")]
pub struct Chat;

#[cfg(feature = "chat")]
impl Extension for Chat {
    const NAME: &'static str = "chat";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.agent(agents::CHAT, || Box::new(agents::Chat::default()));
    }
}

/// For tests: the `echo` agent, and `sleep`, a safe call to interrupt and
/// recover.
#[cfg(any(test, feature = "fixtures"))]
pub struct Fixtures;

#[cfg(any(test, feature = "fixtures"))]
impl Extension for Fixtures {
    const NAME: &'static str = "fixtures";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tools::Sleep)
            .agent(agents::ECHO, || Box::new(agents::Echo));
    }
}

#[cfg(test)]
mod tests;
