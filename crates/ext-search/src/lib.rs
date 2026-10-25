//! `glob` and `grep`, the model's tools over the workspace index the
//! `index` extension keeps (`ext-index`): an extension of their own, so a
//! build or a config can keep the index and its queries without them.

use std::fmt::Write as _;
use std::sync::Arc;

use ext::{Concurrency, Cx, Extension, Json, Need, Registrar, Replay, Schema, Tool};
use ext_index::{
    GrepFile, GrepMode, GrepOptions, GrepQuery, GrepReply, Indexes, Line, Pattern, runtime, watcher,
};
use serde::{Deserialize, Serialize};

/// The `search` extension: `glob` and `grep`, over the `index`
/// extension's indexes.
pub struct Search;

impl ext::Configured for Search {
    fn configure(_: &impl ext::Table) -> Result<Self, ext::Refused> {
        Ok(Self)
    }
}

impl Extension for Search {
    const NAME: &'static str = "search";
    const VERSION: u32 = 1;
    type Needs = (Need<Indexes>, ());

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Glob).tool(Grep);
    }
}

/// The indexes the `index` extension provides.
fn indexes(cx: &Cx) -> Result<Arc<Indexes>, String> {
    cx.service_arc::<Indexes>()
        .ok_or_else(|| "the index extension is not installed".to_owned())
}

/// `glob`: the workspace's files a glob matches.
pub struct Glob;

#[derive(Deserialize, Schema)]
pub struct GlobInput {
    /// A glob relative to the workspace root, such as `src/**/*.rs`; a
    /// pattern without `/` matches file names anywhere.
    pub pattern: String,
}

/// The most paths `glob` returns.
const GLOB_LIMIT: usize = 500;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Globbed {
    /// Most recently modified first.
    pub files: Vec<String>,
    /// How many matched, when more than are listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<usize>,
}

impl Tool for Glob {
    type In = GlobInput;
    type Out = Globbed;

    const NAME: &'static str = "glob";
    const DESCRIPTION: &'static str = "Lists the workspace's files that match a glob, most recently modified first, leaving out what git ignores.";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: Concurrency = Concurrency::Shared;

    async fn call(&self, input: &GlobInput, cx: &Cx) -> Result<Globbed, String> {
        let index = indexes(cx)?.open(
            &cx.workspace,
            runtime(cx.service_arc())?,
            watcher(cx.service_arc())?,
        );
        let found = index.glob(input.pattern.clone(), GLOB_LIMIT).await?;
        Ok(Globbed {
            more: (found.matched > found.files.len()).then_some(found.matched),
            files: found.files.into_iter().map(|f| f.path).collect(),
        })
    }

    fn to_model(output: &Json) -> llm::Output {
        output.parse::<Globbed>().map_or_else(
            |_| llm::Output::Json(output.clone()),
            |globbed| {
                let mut text = globbed.files.join("\n");
                if let Some(more) = globbed.more {
                    let listed = globbed.files.len();
                    let _ = write!(text, "\n[{listed} of {more} files listed]");
                }
                if text.is_empty() {
                    text.push_str("No files matched.");
                }
                llm::Output::Text(text)
            },
        )
    }
}

/// `grep`: the lines of the workspace's files that match.
pub struct Grep;

#[derive(Deserialize, Schema)]
pub struct GrepInput {
    /// The text to find, or a regex when `regex` is set.
    pub pattern: String,
    /// A glob of the files to search, relative to the workspace root.
    pub glob: Option<String>,
    /// Read `pattern` as a regex.
    pub regex: Option<bool>,
    /// Lines of context around each match.
    pub context: Option<usize>,
}

/// The most matches `grep` returns.
const GREP_TOOL_LIMIT: usize = 200;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepOut {
    pub files: Vec<GrepFile>,
    pub reply: GrepReply,
}

impl Tool for Grep {
    type In = GrepInput;
    type Out = GrepOut;

    const NAME: &'static str = "grep";
    const DESCRIPTION: &'static str = "Searches the contents of the workspace's files, leaving out what git ignores. Plain text unless `regex` is set; case-insensitive unless the pattern has a capital.";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: Concurrency = Concurrency::Shared;

    async fn call(&self, input: &GrepInput, cx: &Cx) -> Result<GrepOut, String> {
        let index = indexes(cx)?.open(
            &cx.workspace,
            runtime(cx.service_arc())?,
            watcher(cx.service_arc())?,
        );
        let context = input.context.unwrap_or(0);
        let query = GrepQuery {
            pattern: Pattern::Exact {
                pattern: input.pattern.clone(),
                glob: input.glob.clone(),
            },
            options: GrepOptions {
                mode: if input.regex == Some(true) {
                    GrepMode::Regex
                } else {
                    GrepMode::Plain
                },
                before_context: context,
                after_context: context,
                ..GrepOptions::default()
            },
            limit: GREP_TOOL_LIMIT,
        };
        let mut files = Vec::new();
        let summary = index
            .grep(query, &cx.cancel, |grepped| {
                files.push(GrepFile {
                    path: grepped.path,
                    matches: grepped.matches.into_iter().map(Line::from).collect(),
                });
            })
            .await?;
        Ok(GrepOut {
            files,
            reply: summary.into(),
        })
    }

    /// As ripgrep prints them: `path:line:text`, context as `path-line-text`.
    fn to_model(output: &Json) -> llm::Output {
        output.parse::<GrepOut>().map_or_else(
            |_| llm::Output::Json(output.clone()),
            |out| llm::Output::Text(grep_text(&out)),
        )
    }
}

fn grep_text(out: &GrepOut) -> String {
    let mut text = String::new();
    if let Some(error) = &out.reply.regex_error {
        let _ = writeln!(text, "[not a regex ({error}): searched as plain text]");
    }
    for file in &out.files {
        for m in &file.matches {
            let first = m.line.saturating_sub(m.before.len() as u64);
            for (i, line) in m.before.iter().enumerate() {
                let _ = writeln!(text, "{}-{}-{line}", file.path, first + i as u64);
            }
            let _ = writeln!(text, "{}:{}:{}", file.path, m.line, m.text);
            for (i, line) in m.after.iter().enumerate() {
                let _ = writeln!(text, "{}-{}-{line}", file.path, m.line + 1 + i as u64);
            }
        }
    }
    if out.reply.truncated {
        let _ = writeln!(
            text,
            "[stopped at {} matches; narrow the pattern or the glob]",
            out.reply.matches
        );
    }
    if text.is_empty() {
        text.push_str("No matches.");
    }
    text
}
