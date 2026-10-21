//! `prompt`: the system prompt's one line of identity, and the facts the
//! model is first given about where it runs: the directory, the platform,
//! and the workspace's `AGENTS.md`.

use std::fmt::Write as _;
use std::io::Read;

use ext::{Configured, Extension, Facts, Refused, Registrar, Table, Text};

/// The `prompt` extension: `system`, when set, in place of its line of
/// identity (`[ext.prompt] system`).
#[derive(Clone, Debug, Default)]
pub struct Prompt {
    pub system: Option<String>,
}

const IDENTITY: &str = "You are `a`, a coding agent working in the user's workspace through the tools you are given. Do the work rather than describe it, and keep replies short.";

/// The most of `AGENTS.md` the model is given.
const AGENTS_LIMIT: u64 = 32 * 1024;

/// `[ext.prompt] system`, in place of the identity line; empty is none.
impl Configured for Prompt {
    fn configure(table: &impl Table) -> Result<Self, Refused> {
        let system = table.text("system")?.filter(|s| !s.is_empty());
        Ok(Self {
            system: system.map(str::to_owned),
        })
    }
}

impl Extension for Prompt {
    const NAME: &'static str = "prompt";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let identity = match &self.system {
            Some(system) => Text::system("identity", 0, system.clone()),
            None => Text::system("identity", 0, IDENTITY),
        };
        r.text(identity)
            .text(Text::context("environment", 0, |facts| {
                Some(context(facts))
            }));
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
    let _ = write!(text, "platform: {}\n</environment>", sys::Platform::CURRENT);
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
