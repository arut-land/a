//! The facade the apps link through `BoltFFI` (`boltffi pack apple`): the
//! client and the shared reducer ([`protocol::view`]) as plain owned values
//! and `&self` objects, with no generics or lifetimes to cross the boundary.
//! JSON values (tool input and output, hook decisions) cross as JSON text.
//!
//! A session is the client SDK's [`client::SessionClient`]: the app only
//! renders it. Its updates arrive as a stream that drops items when its
//! buffer is full, so each carries a `rev`: a client that sees a gap asks
//! [`Session::recovery`] to build what it missed off the UI thread. Each update
//! also carries the turn as the SDK computes it ([`TurnInfo`]), the call
//! whose reply is streaming, and each item how it is shown
//! ([`Presentation`]), so no rule is copied into Swift.

mod feed;
mod live;

use std::fmt;
use std::io;

use boltffi::{data, error};
use client::{End, Link, Notice, SessionClient, TurnView};
use protocol::view::{
    Change, Glyph, Item, Kind, Presented, Recap, SessionView, Shown, mismatch_line, recap_line,
};
use protocol::{CommandInfo, SessionSummary};

pub use feed::{Recovery, Reducer};
pub use live::{BlobFetch, Client, QueryStream, Session, SessionWatch, find_session};

/// The purpose of the side request that titles a session, as the app
/// reads its reply from the view.
const TITLE: &str = "title";

/// What went wrong, by kind. [`ClientError::code`] is stable across builds,
/// for clients that branch on it or localize it.
#[error]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// The daemon could not be started or reached, or this build staged.
    Unavailable {
        message: String,
    },
    /// The daemon took the connection but did not answer the hello in time:
    /// it is wedged, and restarting it is the fix.
    NotResponding {
        message: String,
    },
    /// The daemon speaks another protocol. With `newer`, a newer `a` is
    /// staged and running it is the fix; without, updating one side is.
    Mismatch {
        message: String,
        newer: Option<String>,
    },
    /// Admission is full; retry after the outstanding work completes.
    Busy {
        message: String,
    },
    /// The connection is gone; open the session again.
    Closed,
    /// The session is not open: not yet, or no longer.
    NotOpen,
    /// The session does not take this now: a steer or an interrupt while
    /// nothing runs, an answer for a call that is not asking.
    NotOffered {
        message: String,
    },
    /// A message from the daemon, or one handed in, did not decode.
    Protocol {
        message: String,
    },
    InvalidArgument {
        message: String,
    },
}

#[boltffi::data(impl)]
impl ClientError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "unavailable",
            Self::NotResponding { .. } => "not_responding",
            Self::Mismatch { .. } => "mismatch",
            Self::Busy { .. } => "busy",
            Self::Closed => "closed",
            Self::NotOpen => "not_open",
            Self::NotOffered { .. } => "not_offered",
            Self::Protocol { .. } => "protocol",
            Self::InvalidArgument { .. } => "invalid_argument",
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy { message }
            | Self::Unavailable { message }
            | Self::NotResponding { message }
            | Self::Mismatch { message, .. }
            | Self::Protocol { message }
            | Self::NotOffered { message }
            | Self::InvalidArgument { message } => f.write_str(message),
            Self::Closed => f.write_str("the connection to the daemon closed"),
            Self::NotOpen => f.write_str("the session is not open"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<client::Error> for ClientError {
    fn from(e: client::Error) -> Self {
        let message = e.to_string();
        match e {
            client::Error::Mismatch { newer, .. } => Self::Mismatch {
                message,
                newer: newer.map(|p| p.display().to_string()),
            },
            client::Error::NotResponding => Self::NotResponding { message },
            client::Error::Closed => Self::Closed,
            client::Error::Busy => Self::Busy { message },
            client::Error::Fork(_)
            | client::Error::Refused(_)
            | client::Error::Link(_)
            | client::Error::OtherMachine { .. }
            | client::Error::Io(..) => Self::Unavailable { message },
        }
    }
}

impl From<Notice> for ClientError {
    fn from(notice: Notice) -> Self {
        match notice {
            Notice::NotSent(error) => error.into(),
            Notice::Backpressure => Self::Busy {
                message: notice.to_string(),
            },
            Notice::NotConnected => Self::NotOpen,
            notice => Self::NotOffered {
                message: notice.to_string(),
            },
        }
    }
}

/// For what the connection answers with `io::Error` still: settings and
/// the CLI link.
impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        let message = e.to_string();
        match e.kind() {
            io::ErrorKind::TimedOut => Self::NotResponding { message },
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset => Self::Closed,
            io::ErrorKind::InvalidData => Self::Protocol { message },
            io::ErrorKind::InvalidInput => Self::InvalidArgument { message },
            _ => Self::Unavailable { message },
        }
    }
}

/// A session in the daemon's list.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// The full id, 32 hex digits.
    pub id: String,
    /// Its last 8 digits, as the CLI shows it.
    pub short: String,
    pub workspace: String,
    pub entries: u64,
    pub clients: u32,
    /// What it is doing, such as `idle`, `running` or `approval`.
    pub activity: String,
    /// Whether it is `loaded` or `hibernated`.
    pub residency: String,
    /// Whether it waits on the user.
    pub requires_action: bool,
}

/// The daemon's sessions, newest first, as a [`SessionWatch`] sees them
/// change; or, while it cannot reach the daemon, why.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionListUpdate {
    pub sessions: Vec<SessionInfo>,
    pub problem: Option<String>,
}

impl From<&SessionSummary> for SessionInfo {
    fn from(s: &SessionSummary) -> Self {
        Self {
            id: s.id.to_string(),
            short: s.id.short().to_string(),
            workspace: s.workspace.to_string(),
            entries: s.entries,
            clients: s.clients,
            activity: s.activity.name().to_owned(),
            residency: s.residency.name().to_owned(),
            requires_action: s.activity.requires_action(),
        }
    }
}

/// The defaults in `$A_HOME/config.toml`: what a model call uses when its
/// session does not say. The daemon keeps them; see `protocol::HomeSettings`.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `host[@protocol]:model`, such as `openai:gpt-6-luna`.
    pub model: Option<String>,
    /// `loop` (the default) or `echo`.
    pub agent: Option<String>,
    /// Provider variables, such as `OPENAI_API_KEY` and `OPENAI_BASE_URL`.
    pub env: Vec<EnvVar>,
}

impl From<protocol::HomeSettings> for Settings {
    fn from(s: protocol::HomeSettings) -> Self {
        Self {
            model: s.model,
            agent: s.agent,
            env: s
                .env
                .into_iter()
                .map(|(name, value)| EnvVar { name, value })
                .collect(),
        }
    }
}

impl From<Settings> for protocol::HomeSettings {
    fn from(s: Settings) -> Self {
        Self {
            model: s.model,
            agent: s.agent,
            env: s.env.into_iter().map(|v| (v.name, v.value)).collect(),
        }
    }
}

#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// What stands where "Install CLI" would link `a`; see
/// `sys::install::CliLink`.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliStatus {
    /// A link to the staged `runtime/current/a`: the CLI is the daemon's
    /// build.
    Installed { path: String },
    /// A link an earlier install made to another build; replaceable.
    Stale { path: String, target: String },
    /// An `a` installed some other way, left alone.
    Foreign { path: String },
    /// Nothing yet: installing links `path`. `on_path` says whether its
    /// directory is on the `PATH` searched.
    Absent { path: String, on_path: bool },
}

impl From<sys::install::CliLink> for CliStatus {
    fn from(link: sys::install::CliLink) -> Self {
        use sys::install::CliLink;
        let text = |p: std::path::PathBuf| p.display().to_string();
        match link {
            CliLink::Installed { path } => Self::Installed { path: text(path) },
            CliLink::Stale { path, target } => Self::Stale {
                path: text(path),
                target: text(target),
            },
            CliLink::Foreign { path } => Self::Foreign { path: text(path) },
            CliLink::Absent { path, on_path } => Self::Absent {
                path: text(path),
                on_path,
            },
        }
    }
}

/// Which session to open.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    /// A new session working in `workspace` (a directory; its git root is
    /// used).
    New { workspace: String },
    /// The latest session in `workspace`, else in any.
    Latest {
        workspace: String,
        any_workspace: bool,
    },
    /// An existing session, by its full id.
    Id { id: String },
}

/// All display changes from one fold cross the boundary under one revision.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeBatch {
    /// One more than the previous batch; a gap or invalidation requires resynchronization.
    pub revision: u64,
    /// Cached byte charge for the current view and replay cache.
    pub retained_bytes: u64,
    /// The last log entry folded, after this update.
    pub seq: u64,
    pub changes: Vec<ViewChange>,
    /// The turn after this update.
    pub turn: TurnInfo,
    /// The reply attempt streaming after this update. A different identity
    /// replaces the previous attempt's text before applying its deltas.
    pub streaming: Option<StreamId>,
    /// The session's title after this update, once its model named it.
    pub title: Option<String>,
}

/// What the session's turn is doing, and what the controls offer for it;
/// see [`client::TurnView`].
#[data]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
// Flags, as the controls read them.
#[allow(clippy::struct_excessive_bools)]
pub struct TurnInfo {
    /// A call is unsettled.
    pub running: bool,
    /// The oldest call waiting for this client's approval.
    pub asking: Option<u64>,
    pub asks: u32,
    /// An interrupt has something to stop, and a steer a turn to reach.
    pub can_interrupt: bool,
    pub can_send: bool,
}

impl From<TurnView> for TurnInfo {
    fn from(t: TurnView) -> Self {
        Self {
            running: t.running,
            asking: t.asking.map(protocol::CallId::get),
            asks: t.asks,
            can_interrupt: t.can_interrupt,
            can_send: t.can_send,
        }
    }
}

/// A slash command, as help and completion list it.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandHelp {
    /// `/name args`.
    pub usage: String,
    pub help: String,
}

/// What a typed line asks of the app itself, rather than of the session.
#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppAction {
    Help,
    /// Show the session list.
    Sessions,
    NewSession,
}

/// The slash commands every client takes, then those the daemon runs.
pub(crate) fn commands(daemon: &[CommandInfo]) -> Vec<CommandHelp> {
    client::help(daemon)
        .map(|(usage, help)| CommandHelp {
            usage,
            help: help.to_owned(),
        })
        .collect()
}

/// See [`protocol::view::Change`]: an item crosses whole when it is
/// added, and as a patch when it changes.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub enum ViewChange {
    /// The turn controls changed; carried by `ChangeBatch::turn`.
    Activity,
    /// Attached; `reset` when the view started over.
    Attached {
        session: String,
        workspace: String,
        reset: bool,
    },
    Added {
        index: u32,
        item: DisplayItem,
    },
    /// Replaces only the affected display record.
    Updated {
        index: u32,
        item: DisplayItem,
    },
    /// Recover this batch through resync; large presentations are not queued.
    Invalidated {
        added: Vec<u32>,
        updated: Vec<u32>,
    },
    /// A complete view replacement is available through resync.
    ViewInvalidated,
    /// Live text for the reply of `call`.
    Delta {
        call: u64,
        text: String,
    },
    /// The replay is complete, and `recap` tells what it held; what
    /// follows is live.
    Synced {
        recap: Option<RecapInfo>,
    },
    /// The daemon replaced the log-derived view, including its identity.
    Snapshot {
        session: Option<String>,
        workspace: String,
        items: Vec<DisplayItem>,
    },
    /// The daemon is restarting for an upgrade; the session reconnects.
    Draining,
    /// The daemon speaks protocol `supported`, not this build's; `message`
    /// says so as every client does.
    Mismatch {
        supported: u32,
        message: String,
    },
    Error {
        message: String,
    },
    /// The link to the daemon changed.
    Link {
        state: LinkState,
    },
}

/// Display data excludes raw inputs, results and provider state; request details by key.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayItem {
    pub key: u64,
    pub kind: DisplayKind,
    pub presentation: Presentation,
    pub navigation: Option<String>,
}

#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayKind {
    User,
    Assistant,
    Reply,
    Side,
    Tool,
    Command,
    Hook,
    Child,
    Environment,
}

/// How an item is shown, as every client shows it; see
/// [`protocol::view::Presented`].
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presentation {
    pub glyph: StatusGlyph,
    /// One line: what it is, such as `$ ls`; empty for a message.
    pub title: String,
    /// Where it stands, such as `exit 0` or `waiting for approval`; empty
    /// when it has nothing to say.
    pub status: String,
    pub approval: Vec<ApprovalPart>,
    pub question: Option<UserQuestion>,
    pub body: Option<ShownBody>,
    /// A reply's token counts, once it has them.
    pub usage: Option<TokenUsage>,
}

/// A question and its field descriptors; drafts never enter the daemon log.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserQuestion {
    pub call: u64,
    pub id: u64,
    pub source: String,
    pub prompts: Vec<QuestionPrompt>,
}
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionPrompt {
    pub key: String,
    pub message: String,
    pub url: Option<String>,
    pub fields: Vec<QuestionField>,
}
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionField {
    pub key: String,
    pub title: String,
    pub description: Option<String>,
    pub required: bool,
    pub kind: QuestionFieldKind,
    pub options: Vec<QuestionOption>,
    pub initial: Option<String>,
}
#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionFieldKind {
    Text,
    Number,
    Boolean,
    Choice,
    MultipleChoice,
}
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionOption {
    pub value: String,
    pub title: String,
}
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionInput {
    pub key: String,
    pub value: String,
    pub included: bool,
}
#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionAction {
    Accept,
    Decline,
    Cancel,
}

/// One of a query's answers, its JSON as text; nothing follows any but an
/// update.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryAnswer {
    /// A streamed partial answer.
    Update {
        body: String,
    },
    Done {
        body: String,
    },
    /// Why not, the connection's failure included.
    Failed {
        error: String,
    },
    Cancelled,
}

impl From<io::Result<client::Answer>> for QueryAnswer {
    fn from(answer: io::Result<client::Answer>) -> Self {
        use client::Answer;
        use protocol::Outcome;
        match answer {
            Ok(Answer::Update(body)) => Self::Update {
                body: body.to_string(),
            },
            Ok(Answer::Reply(Outcome::Done { output })) => Self::Done {
                body: output.to_string(),
            },
            Ok(Answer::Reply(Outcome::Failed { error })) => Self::Failed { error },
            Ok(Answer::Reply(Outcome::Interrupted { .. })) => Self::Cancelled,
            Err(error) => Self::Failed {
                error: error.to_string(),
            },
        }
    }
}

/// A piece of a blob as it arrives, in order; nothing follows any but
/// bytes.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobPiece {
    Bytes {
        bytes: Vec<u8>,
    },
    /// Every byte asked for came.
    Done,
    /// Refused: no session it was fetched through references the blob.
    NotHeld,
    Cancelled,
    /// Why not: the store's, or the connection's, failure.
    Failed {
        error: String,
    },
}

impl From<&io::Error> for BlobPiece {
    fn from(error: &io::Error) -> Self {
        match client::Unfetched::of(error) {
            Some(client::Unfetched::NotHeld) => Self::NotHeld,
            Some(client::Unfetched::Cancelled) => Self::Cancelled,
            None => Self::Failed {
                error: error.to_string(),
            },
        }
    }
}

/// Ordered approval content from the client SDK.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalPart {
    Call { text: String },
    Reason { text: String },
    UnverifiedSummary { text: String },
    Controls { call: u64 },
}

/// The class of a status: how it is coloured.
#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusGlyph {
    Plain,
    Ok,
    Bad,
    Warn,
    Busy,
    /// Waits on the user.
    Ask,
    /// Refused before it ran.
    Denied,
}

/// What goes under an item's header.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownBody {
    pub kind: BodyKind,
    pub text: String,
}

#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    /// Preformatted text.
    Text,
    /// A unified diff.
    Diff,
    /// A tool's output as JSON text.
    Json,
    /// A reply's markdown.
    Markdown,
}

impl UserQuestion {
    fn from_presented(p: &Presented<'_>) -> Option<Self> {
        p.question.zip(p.question_seq).and_then(|(question, id)| {
            let protocol::view::Buttons::Approve(call) = p.buttons else {
                return None;
            };
            Some(UserQuestion {
                call: call.get(),
                id: id.get(),
                source: question.source.clone(),
                prompts: question
                    .prompts
                    .values()
                    .enumerate()
                    .map(|(pi, prompt)| QuestionPrompt {
                        key: format!("p{pi}"),
                        message: prompt.message().into(),
                        url: if let protocol::Prompt::Url { url, .. } = prompt {
                            Some(url.clone())
                        } else {
                            None
                        },
                        fields: if let protocol::Prompt::Form { fields, .. } = prompt {
                            fields
                                .iter()
                                .enumerate()
                                .map(|(fi, field)| {
                                    let kind = match &field.kind {
                                        protocol::FieldKind::String { .. } => {
                                            QuestionFieldKind::Text
                                        }
                                        protocol::FieldKind::Number { .. } => {
                                            QuestionFieldKind::Number
                                        }
                                        protocol::FieldKind::Boolean => QuestionFieldKind::Boolean,
                                        protocol::FieldKind::Choice {
                                            selection: protocol::Selection::One,
                                            ..
                                        } => QuestionFieldKind::Choice,
                                        protocol::FieldKind::Choice { .. } => {
                                            QuestionFieldKind::MultipleChoice
                                        }
                                    };
                                    QuestionField {
                                        key: format!("f{pi}_{fi}"),
                                        title: field
                                            .title
                                            .clone()
                                            .unwrap_or_else(|| field.name.clone()),
                                        description: field.description.clone(),
                                        required: field.required,
                                        kind,
                                        options: if let protocol::FieldKind::Choice {
                                            options,
                                            ..
                                        } = &field.kind
                                        {
                                            options
                                                .iter()
                                                .map(|o| QuestionOption {
                                                    value: o.value.clone(),
                                                    title: o.title.clone(),
                                                })
                                                .collect()
                                        } else {
                                            Vec::new()
                                        },
                                        initial: field.default_text(),
                                    }
                                })
                                .collect()
                        } else {
                            Vec::new()
                        },
                    })
                    .collect(),
            })
        })
    }
}

impl From<&Presented<'_>> for Presentation {
    fn from(p: &Presented<'_>) -> Self {
        let body = match &p.body {
            Shown::None => None,
            body => Some(ShownBody {
                kind: match body {
                    Shown::Diff(_) => BodyKind::Diff,
                    Shown::Json(_) => BodyKind::Json,
                    Shown::Markdown(_) => BodyKind::Markdown,
                    Shown::None | Shown::Text(_) | Shown::Held(_) => BodyKind::Text,
                },
                text: body.text().into_owned(),
            }),
        };
        Self {
            glyph: match p.glyph {
                Glyph::None => StatusGlyph::Plain,
                Glyph::Ok => StatusGlyph::Ok,
                Glyph::Bad => StatusGlyph::Bad,
                Glyph::Warn => StatusGlyph::Warn,
                Glyph::Busy => StatusGlyph::Busy,
                Glyph::Ask => StatusGlyph::Ask,
                Glyph::Denied => StatusGlyph::Denied,
            },
            title: p.title.to_string(),
            status: p.status.to_string(),
            question: UserQuestion::from_presented(p),
            approval: client::approval_parts(p)
                .into_iter()
                .map(|part| match part {
                    client::ApprovalPart::Call { text } => ApprovalPart::Call { text },
                    client::ApprovalPart::Reason { text } => ApprovalPart::Reason { text },
                    client::ApprovalPart::UnverifiedSummary { text } => {
                        ApprovalPart::UnverifiedSummary { text }
                    }
                    client::ApprovalPart::Controls { call, .. } => {
                        ApprovalPart::Controls { call: call.get() }
                    }
                })
                .collect(),
            body,
            usage: p.usage.map(Into::into),
        }
    }
}

#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecapInfo {
    /// All of it in one line, as every client shows it.
    pub line: String,
    pub prompt: Option<String>,
    pub tools: Vec<String>,
    pub failed: u32,
    pub running: u32,
    pub reply: Option<String>,
    pub asks: u32,
}

/// Where the link to the daemon stands; see [`client::Link`].
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    /// Attached; the replay of what was missed is arriving.
    Replaying,
    Live,
    /// The daemon finishes its work for an upgrade; the session reconnects.
    Draining,
    Reconnecting,
    /// The daemon does not answer: `restart_daemon` replaces it.
    NotResponding {
        who: String,
        log: Vec<String>,
        crash: Option<String>,
    },
    Detached,
    /// The daemon is gone, or would not serve this build; open the
    /// session anew to go on. With `newer`, a newer `a` is staged and
    /// running it is the fix.
    Lost {
        message: String,
        newer: Option<String>,
    },
}

impl LinkState {
    /// Whether the link is over: nothing more will arrive.
    pub const fn ended(&self) -> bool {
        matches!(self, Self::Detached | Self::Lost { .. })
    }
}

impl From<&Link> for LinkState {
    fn from(link: &Link) -> Self {
        match link {
            Link::Connecting => Self::Connecting,
            Link::Replaying { .. } => Self::Replaying,
            Link::Live { .. } => Self::Live,
            Link::Draining => Self::Draining,
            Link::Reconnecting { .. } => Self::Reconnecting,
            Link::Wedged(d) => Self::NotResponding {
                who: d.who(),
                log: d.log_tail(),
                crash: d.crash_note(),
            },
            Link::Ended(End::Detached) => Self::Detached,
            Link::Ended(End::Lost(why)) => Self::Lost {
                message: why.to_string(),
                newer: match why {
                    client::Error::Mismatch { newer, .. } => {
                        newer.as_ref().map(|p| p.display().to_string())
                    }
                    _ => None,
                },
            },
        }
    }
}

#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamId {
    pub call: u64,
    pub attempt: u16,
}

#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamingText {
    pub call: u64,
    pub attempt: u16,
    pub text: String,
}

/// The whole view at `rev`: what a client takes on a gap too large to
/// replay.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub rev: u64,
    pub retained_bytes: u64,
    pub seq: u64,
    pub session: Option<String>,
    pub workspace: String,
    pub items: Vec<DisplayItem>,
    pub link: LinkState,
    pub recap: Option<RecapInfo>,
    pub streaming: Option<StreamingText>,
    pub turn: TurnInfo,
    /// The session's title, once its model named it.
    pub title: Option<String>,
    /// [`protocol::view::SessionView::state_hash`] as 16 hex digits: equal
    /// on every client that folded the same session to the same seq.
    pub state_hash: String,
}

/// Retained changes are replayed or coalesced against the requested revision;
/// a reset or lost replay range requires the whole view.
#[data]
#[derive(Clone, Debug, PartialEq)]
// By value: that is how it crosses the boundary.
#[allow(clippy::large_enum_variant)]
pub enum Resync {
    Updates {
        updates: Vec<ChangeBatch>,
    },
    /// A coalesced replacement for changes since `from`, applied only to that base.
    Patch {
        from: u64,
        update: ChangeBatch,
        streaming: Option<StreamingText>,
    },
    Snapshot {
        snapshot: SessionSnapshot,
    },
}

fn index(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

impl ViewChange {
    /// `change`, which `view` holds now.
    fn of(change: &Change, view: &SessionView) -> Vec<Self> {
        let change = match change {
            Change::Batch { changes } => {
                return changes
                    .iter()
                    .flat_map(|change| Self::of(change, view))
                    .collect();
            }
            Change::Attached {
                session,
                workspace,
                reset,
            } => Self::Attached {
                session: session.to_string(),
                workspace: workspace.to_string(),
                reset: *reset,
            },
            Change::Added { index: i, item } => Self::Added {
                index: index(*i),
                item: DisplayItem::in_view(item, view),
            },
            Change::Updated { index: i, .. } => Self::Updated {
                index: index(*i),
                item: DisplayItem::in_view(&view.items[*i], view),
            },
            Change::Delta { call, text } => Self::Delta {
                call: call.get(),
                text: text.clone(),
            },
            Change::Activity => Self::Activity,
            Change::Synced => Self::Synced {
                recap: view.recap.as_ref().map(Into::into),
            },
            Change::Snapshot => Self::Snapshot {
                session: view.session.map(|s| s.to_string()),
                workspace: view.workspace.to_string(),
                items: view
                    .items
                    .iter()
                    .map(|item| DisplayItem::in_view(item, view))
                    .collect(),
            },
            Change::Draining => Self::Draining,
            Change::Mismatch { supported } => Self::Mismatch {
                supported: *supported,
                message: mismatch_line(*supported),
            },
            Change::Error { message } => Self::Error {
                message: message.clone(),
            },
        };
        vec![change]
    }
}

impl From<protocol::view::Usage> for TokenUsage {
    fn from(u: protocol::view::Usage) -> Self {
        Self {
            input: u.input,
            output: u.output,
        }
    }
}

impl DisplayItem {
    fn in_view(item: &Item, view: &SessionView) -> Self {
        let shown = view.present(item);
        Self {
            key: item.key().get(),
            kind: match shown.kind {
                Kind::User => DisplayKind::User,
                Kind::Assistant => DisplayKind::Assistant,
                Kind::Reply => DisplayKind::Reply,
                Kind::Side => DisplayKind::Side,
                Kind::Tool => DisplayKind::Tool,
                Kind::Command => DisplayKind::Command,
                Kind::Hook => DisplayKind::Hook,
                Kind::Child => DisplayKind::Child,
                Kind::Environment => DisplayKind::Environment,
            },
            presentation: Presentation::from(&shown),
            navigation: match item {
                Item::Child { child, .. } => Some(child.to_string()),
                _ => None,
            },
        }
    }
}

impl From<&Recap> for RecapInfo {
    fn from(r: &Recap) -> Self {
        Self {
            line: recap_line(r),
            prompt: r.prompt.clone(),
            tools: r.tools.clone(),
            failed: r.failed,
            running: r.running,
            reply: r.reply.clone(),
            asks: r.asks,
        }
    }
}

impl SessionSnapshot {
    fn of(client: &SessionClient, rev: u64, retained_bytes: u64) -> Self {
        let view = client.view();
        Self::with_items(
            view,
            rev,
            retained_bytes,
            client.link().into(),
            client.turn().into(),
            view.items
                .iter()
                .map(|item| DisplayItem::in_view(item, view))
                .collect(),
        )
    }

    fn with_items(
        view: &SessionView,
        rev: u64,
        retained_bytes: u64,
        link: LinkState,
        turn: TurnInfo,
        items: Vec<DisplayItem>,
    ) -> Self {
        Self {
            rev,
            retained_bytes,
            seq: view.seq,
            session: view.session.map(|s| s.to_string()),
            workspace: view.workspace.to_string(),
            items,
            link,
            recap: view.recap.as_ref().map(Into::into),
            streaming: view.streaming.as_ref().map(|s| StreamingText {
                call: s.call.get(),
                attempt: s.attempt,
                text: s.text.clone(),
            }),
            turn,
            title: view.side(TITLE).map(str::to_owned),
            state_hash: format!("{:016x}", view.state_hash()),
        }
    }
}
