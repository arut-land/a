//! One workspace index in the daemon, shared by every client and session
//! working there, over a pruned copy of FFF's search core (`fff`):
//!
//! - `files.search`, a workspace query: the best files for what is being
//!   typed, as an @-mention completes, at once.
//! - `files.grep`, a workspace query: each file's matches streamed as it
//!   is read, until the limit or a cancel.
//! - `glob` and `grep`, the model's tools over the same index.
//!
//! The index of a workspace ([`WorkspaceIndex`]) opens with the first of these
//! that asks about it and closes once none has for [`Limits::linger`]: a
//! daemon nobody searches holds none.

use std::future::Future;
use std::sync::Arc;

use ext::query::Workspace;
use ext::{
    Concurrency, Configured, Cx, Extension, Json, Partials, Query, QueryCx, Refused, Registrar,
    Replay, Schema, Streamed, Table, Tool, stream,
};
use serde::{Deserialize, Serialize};

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]
pub(crate) mod fff;
mod index;

pub use fff::grep::{Casing, GrepMatch, GrepMode, GrepOptions};
pub use index::{
    GrepQuery, GrepSummary, Grepped, Indexes, Limits, Pattern, Searched, SearchedFile,
    WorkspaceIndex,
};

/// The `index` extension: its queries and tools share one [`Indexes`].
pub struct Index {
    indexes: Arc<Indexes>,
}

impl Configured for Index {
    fn configure(table: &impl Table) -> Result<Self, Refused> {
        let mut limits = Limits::default();
        if let Some(mib) = table.whole("memory_mib")? {
            let bytes = usize::try_from(mib)
                .ok()
                .and_then(|mib| mib.checked_mul(1 << 20))
                .ok_or_else(|| Refused(format!("ext.{}.memory_mib is too large", table.name())))?;
            limits.memory = bytes;
        }
        Ok(Self {
            indexes: Arc::new(Indexes::new(limits)),
        })
    }
}

impl Extension for Index {
    const NAME: &'static str = "index";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let indexes = &self.indexes;
        r.query(FileSearch(Arc::clone(indexes)))
            .query(FileGrep(Arc::clone(indexes)))
            .tool(Glob(Arc::clone(indexes)))
            .tool(GrepTool(Arc::clone(indexes)));
    }
}

/// The runtime the engine runs a query or call on.
fn runtime(service: Option<Arc<dyn rt::Runtime>>) -> Result<rt::Rt, String> {
    service.ok_or_else(|| "the engine provides no runtime".to_owned())
}

/// `files.search`: the best files of a workspace for `query`.
pub struct FileSearch(Arc<Indexes>);

#[derive(Deserialize)]
pub struct SearchRequest {
    /// What is typed: fuzzy text, with `*.rs`, `src/`, `!test` and globs.
    pub query: String,
    #[serde(default = "search_limit")]
    pub limit: usize,
}

fn search_limit() -> usize {
    20
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchReply {
    pub files: Vec<FoundFile>,
    /// How many files matched at all.
    pub matched: usize,
    /// How many files the workspace has.
    pub total: usize,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FoundFile {
    /// Relative to the workspace, `/`-separated.
    pub path: String,
    /// Byte ranges of `path` that matched, to highlight.
    pub ranges: Vec<(u32, u32)>,
}

impl Query for FileSearch {
    const NAME: &'static str = "files.search";
    const DESCRIPTION: &'static str =
        "The workspace's files that best match what is typed, best first.";
    type Scope = Workspace;
    type Request = SearchRequest;
    type Reply = SearchReply;

    fn answer(
        &self,
        request: SearchRequest,
        cx: &QueryCx<Workspace>,
    ) -> impl Future<Output = Result<SearchReply, String>> + Send {
        let rt = runtime(cx.service_arc::<dyn rt::Runtime>());
        let indexes = Arc::clone(&self.0);
        let root = Arc::clone(&cx.scope.path);
        async move {
            let index = indexes.open(&root, rt?);
            let found = index.search(request.query, request.limit).await;
            Ok(SearchReply {
                files: (found.files.into_iter())
                    .map(|f| FoundFile {
                        path: f.path,
                        ranges: f.ranges,
                    })
                    .collect(),
                matched: found.matched,
                total: found.total,
            })
        }
    }
}

/// `files.grep`: the lines of a workspace's files that match, a file's
/// matches an update each.
pub struct FileGrep(Arc<Indexes>);

#[derive(Deserialize)]
pub struct GrepRequest {
    /// The pattern, with the constraints `files.search` takes.
    pub query: String,
    #[serde(default)]
    pub mode: GrepMode,
    #[serde(default)]
    pub case: Casing,
    /// Lines shown before and after each match.
    #[serde(default)]
    pub context: usize,
    #[serde(default = "grep_limit")]
    pub limit: usize,
}

fn grep_limit() -> usize {
    500
}

/// One file's matches.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepFile {
    pub path: String,
    pub matches: Vec<Line>,
}

/// A matched line.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Line {
    pub line: u64,
    pub text: String,
    pub ranges: Vec<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

impl From<GrepMatch> for Line {
    fn from(m: GrepMatch) -> Self {
        Self {
            line: m.line,
            text: m.text,
            ranges: m.ranges.into_vec(),
            before: m.before,
            after: m.after,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepReply {
    /// Files read.
    pub searched: usize,
    /// Files with a match.
    pub files: usize,
    pub matches: usize,
    /// It stopped at its limit with files left to read.
    pub truncated: bool,
    /// Why the regex asked for was searched as literal text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex_error: Option<String>,
}

impl From<GrepSummary> for GrepReply {
    fn from(summary: GrepSummary) -> Self {
        Self {
            searched: summary.searched,
            files: summary.matched_files,
            matches: summary.matches,
            truncated: summary.truncated,
            regex_error: summary.regex_error,
        }
    }
}

impl Query for FileGrep {
    const NAME: &'static str = "files.grep";
    const DESCRIPTION: &'static str =
        "The lines of the workspace's files that match, a file's matches at a time.";
    type Scope = Workspace;
    type Request = GrepRequest;
    type Reply = GrepReply;

    fn answer(
        &self,
        request: GrepRequest,
        cx: &QueryCx<Workspace>,
    ) -> Streamed<GrepFile, impl Future<Output = Result<GrepReply, String>> + Send, GrepReply> {
        let rt = runtime(cx.service_arc::<dyn rt::Runtime>());
        let indexes = Arc::clone(&self.0);
        let root = Arc::clone(&cx.scope.path);
        let cancel = cx.cancel.clone();
        stream(move |updates: Partials<GrepFile>| async move {
            let index = indexes.open(&root, rt?);
            let query = GrepQuery {
                pattern: Pattern::Typed(request.query),
                options: GrepOptions {
                    mode: request.mode,
                    casing: request.case,
                    before_context: request.context,
                    after_context: request.context,
                    ..GrepOptions::default()
                },
                limit: request.limit,
            };
            let summary = index
                .grep(query, &cancel, |grepped| {
                    updates.send(GrepFile {
                        path: grepped.path,
                        matches: grepped.matches.into_iter().map(Line::from).collect(),
                    });
                })
                .await?;
            Ok(summary.into())
        })
    }
}

/// `glob`: the workspace's files a glob matches.
pub struct Glob(Arc<Indexes>);

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
        let index = self.0.open(&cx.workspace, runtime(cx.service_arc())?);
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
                    text.push_str(&format!(
                        "\n[{} of {more} files listed]",
                        globbed.files.len()
                    ));
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
pub struct GrepTool(Arc<Indexes>);

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

impl Tool for GrepTool {
    type In = GrepInput;
    type Out = GrepOut;

    const NAME: &'static str = "grep";
    const DESCRIPTION: &'static str = "Searches the contents of the workspace's files, leaving out what git ignores. Plain text unless `regex` is set; case-insensitive unless the pattern has a capital.";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: Concurrency = Concurrency::Shared;

    async fn call(&self, input: &GrepInput, cx: &Cx) -> Result<GrepOut, String> {
        let index = self.0.open(&cx.workspace, runtime(cx.service_arc())?);
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
        let files = std::sync::Mutex::new(Vec::new());
        let summary = index
            .grep(query, &cx.cancel, |grepped| {
                files
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(GrepFile {
                        path: grepped.path,
                        matches: grepped.matches.into_iter().map(Line::from).collect(),
                    });
            })
            .await?;
        Ok(GrepOut {
            files: files
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
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
    use std::fmt::Write as _;
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
