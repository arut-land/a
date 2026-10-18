//! The facade the apps link through `BoltFFI` (`boltffi pack apple`): the
//! client and the shared reducer ([`protocol::view`]) as plain owned values
//! and `&self` objects, with no generics or lifetimes to cross the boundary.
//! JSON values (tool input and output, hook decisions) cross as JSON text.
//!
//! Session updates arrive as a stream that drops items when its buffer is
//! full, so each carries the reducer's `rev`: a client that sees a gap asks
//! [`Session::resync`] for what it missed, or a fresh snapshot.

mod feed;
mod live;

use std::fmt;
use std::io;

use boltffi::{data, error};
use protocol::view::{self, Approval, Change, Item, Link, Part, SessionView, Status};
use protocol::{Outcome, Recap, SessionSummary};

pub use feed::Reducer;
pub use live::{Client, Session};

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
    /// The connection is gone; open the session again.
    Closed,
    /// The session is not open: not yet, or no longer.
    NotOpen,
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
            Self::Closed => "closed",
            Self::NotOpen => "not_open",
            Self::Protocol { .. } => "protocol",
            Self::InvalidArgument { .. } => "invalid_argument",
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { message }
            | Self::NotResponding { message }
            | Self::Protocol { message }
            | Self::InvalidArgument { message } => f.write_str(message),
            Self::Closed => f.write_str("the connection to the daemon closed"),
            Self::NotOpen => f.write_str("the session is not open"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        let message = e.to_string();
        match e.kind() {
            io::ErrorKind::TimedOut => Self::NotResponding { message },
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset => Self::Closed,
            io::ErrorKind::InvalidData => Self::Protocol { message },
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
}

impl From<&SessionSummary> for SessionInfo {
    fn from(s: &SessionSummary) -> Self {
        Self {
            id: s.id.to_string(),
            short: s.id.short().to_string(),
            workspace: s.workspace.display().to_string(),
            entries: s.entries,
            clients: s.clients,
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

/// One change to a session's view, numbered.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub struct ViewUpdate {
    /// One more than the previous update's; a gap means some were dropped.
    pub rev: u64,
    /// The last log entry folded, after this update.
    pub seq: u64,
    pub change: ViewChange,
}

/// See [`protocol::view::Change`]; items are upserted whole by index.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub enum ViewChange {
    /// Attached; `reset` when the view started over.
    Attached {
        session: String,
        workspace: String,
        reset: bool,
    },
    Added {
        index: u32,
        item: ViewItem,
    },
    Updated {
        index: u32,
        item: ViewItem,
        part: ItemPart,
    },
    /// Live text for the reply of `call`.
    Delta {
        call: u64,
        text: String,
    },
    /// This client is asked to approve the tool call `item`, at `index`
    /// (inserted there if the view did not show it yet).
    Asked {
        index: u32,
        item: ViewItem,
    },
    /// The replay is complete; what follows is live.
    Synced,
    Recap {
        recap: RecapInfo,
    },
    /// The daemon is restarting for an upgrade; the session reconnects.
    Draining,
    Mismatch {
        supported: u32,
    },
    Error {
        message: String,
    },
    /// The connection ended for good; open the session anew to go on.
    Closed {
        message: String,
    },
    /// This client detached.
    Detached,
}

/// A transcript item; see [`protocol::view::Item`].
#[data]
#[derive(Clone, Debug, PartialEq)]
pub enum ViewItem {
    User {
        seq: u64,
        text: String,
    },
    Assistant {
        seq: u64,
        text: String,
    },
    /// A model call and the reply it got.
    Reply {
        call: u64,
        model: String,
        text: String,
        status: ReplyStatus,
        usage: Option<TokenUsage>,
    },
    Tool {
        call: u64,
        tool: String,
        /// JSON.
        input_json: String,
        approval: Option<ApprovalState>,
        outcome: Option<ToolOutcome>,
        /// How the tool presents the call, or once it has one, its outcome.
        card: Option<ToolCard>,
        /// Whether this client was asked to approve it.
        asked: bool,
    },
    /// A slash command and its outcome.
    Command {
        call: u64,
        name: String,
        args: String,
        outcome: Option<ToolOutcome>,
    },
    Hook {
        seq: u64,
        hook: String,
        /// JSON.
        decision_json: String,
    },
    Child {
        call: u64,
        child: String,
        input: String,
        watch: Option<u64>,
        fired: Option<ToolOutcome>,
    },
    Environment {
        seq: u64,
        cwd: String,
    },
}

#[boltffi::data(impl)]
impl ViewItem {
    /// The seq of the entry that started this item: items are in its order.
    pub fn key(&self) -> u64 {
        match self {
            Self::User { seq, .. }
            | Self::Assistant { seq, .. }
            | Self::Hook { seq, .. }
            | Self::Environment { seq, .. } => *seq,
            Self::Reply { call, .. }
            | Self::Tool { call, .. }
            | Self::Child { call, .. }
            | Self::Command { call, .. } => *call,
        }
    }
}

/// How a tool call or its outcome is shown; see [`protocol::Card`].
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCard {
    pub title: String,
    /// Preformatted text, such as output or a diff; may be empty.
    pub body: String,
    /// A short status, such as `exit 1`; may be empty.
    pub note: String,
}

#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyStatus {
    Running,
    Done,
    Failed { error: String },
    Interrupted,
}

#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
}

#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemPart {
    Outcome,
    Approval,
    Watch,
}

#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    /// `output_json` is the tool's output as JSON.
    Done {
        output_json: String,
    },
    Failed {
        error: String,
    },
    /// The daemon stopped mid-call and the tool is not safe to rerun.
    Interrupted,
}

/// What happened in a session, sent on attach.
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecapInfo {
    pub prompt: Option<String>,
    pub tools: Vec<String>,
    pub failed: u32,
    pub running: u32,
    pub reply: Option<String>,
    pub asks: u32,
}

#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    Replaying,
    Live,
    Draining,
    Closed,
}

#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamingText {
    pub call: u64,
    pub text: String,
}

/// The whole view at `rev`: what a client takes on a gap too large to
/// replay.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub rev: u64,
    pub seq: u64,
    pub session: Option<String>,
    pub workspace: String,
    pub items: Vec<ViewItem>,
    pub link: LinkState,
    pub recap: Option<RecapInfo>,
    pub streaming: Option<StreamingText>,
    /// [`SessionView::state_hash`] as 16 hex digits: equal on every client
    /// that folded the same session to the same seq.
    pub state_hash: String,
}

/// The answer to a resync: the updates after the one asked from when they
/// are still kept, else the whole view.
#[data]
#[derive(Clone, Debug, PartialEq)]
// By value: that is how it crosses the boundary.
#[allow(clippy::large_enum_variant)]
pub enum Resync {
    Updates { updates: Vec<ViewUpdate> },
    Snapshot { snapshot: SessionSnapshot },
}

fn index(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

impl From<&view::Update> for ViewUpdate {
    fn from(u: &view::Update) -> Self {
        Self {
            rev: u.rev,
            seq: u.seq,
            change: (&u.change).into(),
        }
    }
}

impl From<&Change> for ViewChange {
    fn from(change: &Change) -> Self {
        match change {
            Change::Attached {
                session,
                workspace,
                reset,
            } => Self::Attached {
                session: session.to_string(),
                workspace: workspace.display().to_string(),
                reset: *reset,
            },
            Change::Added { index: i, item } => Self::Added {
                index: index(*i),
                item: item.into(),
            },
            Change::Updated {
                index: i,
                item,
                part,
            } => Self::Updated {
                index: index(*i),
                item: item.into(),
                part: match part {
                    Part::Outcome => ItemPart::Outcome,
                    Part::Approval => ItemPart::Approval,
                    Part::Watch => ItemPart::Watch,
                },
            },
            Change::Delta { call, text } => Self::Delta {
                call: *call,
                text: text.clone(),
            },
            Change::Asked { index: i, item } => Self::Asked {
                index: index(*i),
                item: item.into(),
            },
            Change::Synced => Self::Synced,
            Change::Recap(recap) => Self::Recap {
                recap: recap.into(),
            },
            Change::Draining => Self::Draining,
            Change::Mismatch { supported } => Self::Mismatch {
                supported: *supported,
            },
            Change::Error { message } => Self::Error {
                message: message.clone(),
            },
        }
    }
}

impl From<&Item> for ViewItem {
    fn from(item: &Item) -> Self {
        match item {
            Item::User { seq, text } => Self::User {
                seq: *seq,
                text: text.clone(),
            },
            Item::Assistant { seq, text } => Self::Assistant {
                seq: *seq,
                text: text.clone(),
            },
            Item::Reply {
                call,
                model,
                text,
                status,
                usage,
            } => Self::Reply {
                call: *call,
                model: model.clone(),
                text: text.clone(),
                status: match status {
                    Status::Running => ReplyStatus::Running,
                    Status::Done => ReplyStatus::Done,
                    Status::Failed { error } => ReplyStatus::Failed {
                        error: error.clone(),
                    },
                    Status::Interrupted => ReplyStatus::Interrupted,
                },
                usage: usage.map(|u| TokenUsage {
                    input: u.input,
                    output: u.output,
                }),
            },
            Item::Tool {
                call,
                tool,
                input,
                approval,
                outcome,
                card,
                asked,
            } => Self::Tool {
                call: *call,
                tool: tool.clone(),
                input_json: input.to_string(),
                approval: approval.map(|a| match a {
                    Approval::Pending => ApprovalState::Pending,
                    Approval::Approved => ApprovalState::Approved,
                    Approval::Denied => ApprovalState::Denied,
                }),
                outcome: outcome.as_ref().map(Into::into),
                card: card.as_ref().map(|c| ToolCard {
                    title: c.title.clone(),
                    body: c.body.clone(),
                    note: c.note.clone(),
                }),
                asked: *asked,
            },
            Item::Command {
                call,
                name,
                args,
                outcome,
            } => Self::Command {
                call: *call,
                name: name.clone(),
                args: args.clone(),
                outcome: outcome.as_ref().map(Into::into),
            },
            Item::Hook {
                seq,
                hook,
                decision,
            } => Self::Hook {
                seq: *seq,
                hook: hook.clone(),
                decision_json: serde_json::to_string(decision).unwrap_or_default(),
            },
            Item::Child {
                call,
                child,
                input,
                watch,
                fired,
            } => Self::Child {
                call: *call,
                child: child.to_string(),
                input: input.clone(),
                watch: *watch,
                fired: fired.as_ref().map(Into::into),
            },
            Item::Environment { seq, cwd } => Self::Environment {
                seq: *seq,
                cwd: cwd.display().to_string(),
            },
        }
    }
}

impl From<&Outcome> for ToolOutcome {
    fn from(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Done { output } => Self::Done {
                output_json: output.to_string(),
            },
            Outcome::Failed { error } => Self::Failed {
                error: error.clone(),
            },
            Outcome::Interrupted => Self::Interrupted,
        }
    }
}

impl From<&Recap> for RecapInfo {
    fn from(r: &Recap) -> Self {
        Self {
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
    fn of(view: &SessionView, closed: bool) -> Self {
        Self {
            rev: view.rev,
            seq: view.seq,
            session: view.session.map(|s| s.to_string()),
            workspace: view.workspace.display().to_string(),
            items: view.items.iter().map(Into::into).collect(),
            link: match view.link {
                _ if closed => LinkState::Closed,
                Link::Connecting => LinkState::Connecting,
                Link::Replaying => LinkState::Replaying,
                Link::Live => LinkState::Live,
                Link::Draining => LinkState::Draining,
            },
            recap: view.recap.as_ref().map(Into::into),
            streaming: view.streaming.as_ref().map(|s| StreamingText {
                call: s.call,
                text: s.text.clone(),
            }),
            state_hash: format!("{:016x}", view.state_hash()),
        }
    }
}
