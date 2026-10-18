//! The built-in extensions. Each registers through the same verbs as any
//! other and can be replaced by one of the same name or left out.

use std::fmt::Write as _;
use std::io::Read;

use crate::ext::{Extension, Facts, Refused, Registrar, Registry, Section, Slot};
use crate::state::Cwd;
use crate::tools::{self, edit::Seen};
use crate::{AskFor, Tool};

/// The built-ins in their install order, with `policy` as configured.
pub fn builtins(policy: &Policy) -> Result<Registry, Refused> {
    Registry::default()
        .with(&Prompt)?
        .with(&Shell)?
        .with(&Edit)?
        .with(policy)
}

/// The system prompt's one line of identity, and the facts the model is
/// first given about where it runs: the directory, the platform, and the
/// workspace's `AGENTS.md`.
pub struct Prompt;

/// The most of `AGENTS.md` the model is given.
const AGENTS_LIMIT: u64 = 32 * 1024;

impl Extension for Prompt {
    const NAME: &'static str = "prompt";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.prompt_section(Section {
            name: "identity",
            order: 0,
            text: "You are `a`, a coding agent working in the user's workspace through the tools you are given. Do the work rather than describe it, and keep replies short.".to_owned(),
            tool: None,
        })
        .context(|facts| Some(context(facts)));
    }
}

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

/// `bash`, and the session's working directory it keeps. It fills the
/// spawn slot: processes tools start are its to run.
pub struct Shell;

impl Extension for Shell {
    const NAME: &'static str = "shell";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tools::Bash)
            .state::<Cwd>()
            .claim(Slot::Spawn)
            .prompt_section(Section {
                name: "bash",
                order: 10,
                text: "`bash` runs one command per call in a fresh shell whose working directory persists between calls (`cd` sticks; variables do not). There is no stdin or terminal: use flags that avoid prompts. Long output is cut to its start and end, with the whole saved to a file the output names: search it with `grep` or `sed -n` rather than reading it all. Set `timeout` (seconds, 120 by default, at most 600) for slow commands, and `background: true` for servers and watchers, then read their log.".to_owned(),
                tool: Some(tools::Bash::NAME),
            });
    }
}

/// `edit`, and the hashes it keeps to catch stale reads.
pub struct Edit;

impl Extension for Edit {
    const NAME: &'static str = "edit";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tools::Edit)
            .state::<Seen>()
            .prompt_section(Section {
                name: "edit",
                order: 20,
                text: "`edit` changes a file by exact replacement: each `old` must appear exactly once in the file as it was before the call, so include enough of the text around it, and the edits of one call must not overlap. One edit with an empty `old` writes the whole file. Look at a file before editing it: an edit to a file that changed since this session last edited it fails until you look again.".to_owned(),
                tool: Some(tools::Edit::NAME),
            });
    }
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

impl Extension for Policy {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on(AskFor(self.ask.clone()));
    }
}

/// Tools for tests: `sleep`, a safe call to interrupt and recover.
pub struct Fixtures;

impl Extension for Fixtures {
    const NAME: &'static str = "fixtures";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(tools::Sleep);
    }
}
