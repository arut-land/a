//! The facade the apps link through `BoltFFI` (`boltffi pack apple`): the
//! client and the shared reducer ([`protocol::view`]) as plain owned values
//! and `&self` objects, with no generics or lifetimes to cross the boundary.
//! JSON values (tool input and output, hook decisions) cross as JSON text.
//!
//! A session is the client SDK's [`client::SessionClient`]: the app only
//! renders it. Its updates arrive as a stream that drops items when its
//! buffer is full, so each carries a `rev`: a client that sees a gap asks
//! [`Session::resync`] for what it missed, or a fresh snapshot. Each update
//! also carries the turn as the SDK computes it ([`TurnInfo`]), the call
//! whose reply is streaming, and each tool call its [`CallPhase`], so no
//! rule is copied into Swift.

mod feed;
mod live;

use std::fmt;
use std::io;

use boltffi::{data, error};
use client::{End, Link, Notice, SessionClient, TurnView};
use protocol::view::{
    Approval, Change, Item, Part, Phase, Recap, SessionView, Status, call_title, decision_line,
    output_text, recap_line,
};
use protocol::{Card, CommandInfo, Outcome, SessionSummary};

pub use feed::Reducer;
pub use live::{Client, Session, SessionWatch, find_session};

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
            Self::Unavailable { message }
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
            client::Error::Refused(_) | client::Error::Io(..) => Self::Unavailable { message },
        }
    }
}

impl From<Notice> for ClientError {
    fn from(notice: Notice) -> Self {
        match notice {
            Notice::NotConnected | Notice::NotSent(_) => Self::NotOpen,
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
            workspace: s.workspace.display().to_string(),
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

/// As the pane shows it: names trimmed, blank rows skipped. The daemon,
/// the one writer, refuses a name a shell could not export.
impl From<Settings> for protocol::HomeSettings {
    fn from(s: Settings) -> Self {
        Self {
            model: s.model,
            agent: s.agent,
            env: s
                .env
                .into_iter()
                .map(|EnvVar { name, value }| (name.trim().to_owned(), value))
                .filter(|(name, _)| !name.is_empty())
                .collect(),
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

/// One change to a session's view, numbered.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub struct ViewUpdate {
    /// One more than the previous update's; a gap means some were dropped.
    pub rev: u64,
    /// The last log entry folded, after this update.
    pub seq: u64,
    pub change: ViewChange,
    /// The turn after this update.
    pub turn: TurnInfo,
    /// The call whose reply is streaming in after this update: the text
    /// shown for it is the deltas since it became this.
    pub streaming: Option<u64>,
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
            asking: t.asking,
            asks: t.asks,
            can_interrupt: t.can_interrupt,
            can_send: t.can_send,
        }
    }
}

/// Where a model call, tool call or command stands; see
/// [`protocol::view::Phase`].
#[data]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallPhase {
    Running,
    /// Waits for an approval: show the buttons.
    Asking,
    Denied,
    Done,
    Failed,
    Interrupted,
}

impl From<Phase> for CallPhase {
    fn from(p: Phase) -> Self {
        match p {
            Phase::Running => Self::Running,
            Phase::Asking => Self::Asking,
            Phase::Denied => Self::Denied,
            Phase::Done => Self::Done,
            Phase::Failed => Self::Failed,
            Phase::Interrupted => Self::Interrupted,
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

/// See [`protocol::view::Change`]: an item is added whole, then patched.
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
    /// The item at `index` changed as `patch` says; the rest of it stands.
    Updated {
        index: u32,
        patch: ItemPatch,
    },
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
    /// The daemon sent the whole view: the items are these now.
    Snapshot {
        items: Vec<ViewItem>,
    },
    /// The daemon is restarting for an upgrade; the session reconnects.
    Draining,
    Mismatch {
        supported: u32,
    },
    Error {
        message: String,
    },
    /// The link to the daemon changed.
    Link {
        state: LinkState,
    },
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
    /// A model call and the reply it got. A side request an extension
    /// made names its `purpose`: its reply is the extension's (a title is
    /// the session's [`SessionSnapshot::title`]), not the conversation's.
    Reply {
        call: u64,
        model: String,
        text: String,
        status: ReplyStatus,
        usage: Option<TokenUsage>,
        purpose: Option<String>,
    },
    Tool {
        call: u64,
        tool: String,
        /// JSON.
        input_json: String,
        /// The call in one line, as every client shows it.
        title: String,
        approval: Option<ApprovalState>,
        outcome: Option<ToolOutcome>,
        /// How the tool presents the call, or once it has one, its outcome.
        card: Option<ToolCard>,
        phase: CallPhase,
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
        /// The decision in one line, as every client shows it.
        line: String,
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

/// What changed in an item; see [`protocol::view::Part`]. A call's phase
/// comes with the part that moved it.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub enum ItemPatch {
    /// A tool's, a command's or a child's outcome, with how the tool
    /// presents the call now: its card, and its title (which follows the
    /// card).
    Outcome {
        outcome: ToolOutcome,
        title: String,
        card: Option<ToolCard>,
        phase: CallPhase,
    },
    /// A reply's text and status, with what its model call used.
    Reply {
        text: String,
        status: ReplyStatus,
        usage: Option<TokenUsage>,
    },
    Approval {
        approval: ApprovalState,
        phase: CallPhase,
    },
    /// A child's watch, fired or not.
    Watch { watch: Option<u64> },
    /// The item as it is now: a typed line became the slash command it
    /// ran, which then got its outcome.
    Command { item: ViewItem },
}

#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutcome {
    /// `output_json` is the tool's output as JSON; `shown` is it as text,
    /// as every client shows it, when the tool or the output says how.
    Done {
        output_json: String,
        shown: Option<OutputText>,
    },
    Failed {
        error: String,
    },
    /// The daemon stopped mid-call and the tool is not safe to rerun.
    Interrupted,
}

/// A finished call's output as text, and a short note on it such as
/// `exit 1`; see [`protocol::view::output_text`].
#[data]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputText {
    pub text: String,
    pub note: String,
}

/// What happened in a session, told on attach.
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
    pub turn: TurnInfo,
    /// The session's title, once its model named it.
    pub title: Option<String>,
    /// [`protocol::view::SessionView::state_hash`] as 16 hex digits: equal
    /// on every client that folded the same session to the same seq.
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

impl ViewChange {
    /// `change`, which `view` holds now.
    fn of(change: &Change, view: &SessionView) -> Self {
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
                patch: ItemPatch::of(item, *part),
            },
            Change::Delta { call, text } => Self::Delta {
                call: *call,
                text: text.clone(),
            },
            Change::Synced => Self::Synced {
                recap: view.recap.as_ref().map(Into::into),
            },
            Change::Snapshot { items } => Self::Snapshot {
                items: items.iter().map(Into::into).collect(),
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

impl ItemPatch {
    /// What `part` of `item` is now.
    fn of(item: &Item, part: Part) -> Self {
        let phase = || item.phase().map_or(CallPhase::Running, Into::into);
        match (item, part) {
            (
                Item::Tool {
                    by: protocol::Audience::User,
                    ..
                },
                _,
            ) => Self::Command { item: item.into() },
            (
                Item::Tool {
                    approval: Some(approval),
                    ..
                },
                Part::Approval,
            ) => Self::Approval {
                approval: (*approval).into(),
                phase: phase(),
            },
            (Item::Child { watch, .. }, Part::Watch) => Self::Watch { watch: *watch },
            (
                Item::Reply {
                    text,
                    status,
                    usage,
                    ..
                },
                Part::Outcome,
            ) => Self::Reply {
                text: text.clone(),
                status: status.into(),
                usage: usage.map(Into::into),
            },
            (
                Item::Tool {
                    tool,
                    input,
                    outcome,
                    card,
                    ..
                },
                _,
            ) => Self::Outcome {
                outcome: outcome
                    .as_ref()
                    .map_or(ToolOutcome::Interrupted, |o| tool_outcome(o, card.as_ref())),
                title: call_title(tool, input, card.as_ref()),
                card: card.as_ref().map(Into::into),
                phase: phase(),
            },
            (Item::Child { fired, .. }, _) => Self::Outcome {
                outcome: fired
                    .as_ref()
                    .map_or(ToolOutcome::Interrupted, |o| tool_outcome(o, None)),
                title: String::new(),
                card: None,
                phase: phase(),
            },
            // Nothing else is ever updated.
            (item, part) => unreachable!("{part:?} of {item:?}"),
        }
    }
}

impl From<Approval> for ApprovalState {
    fn from(a: Approval) -> Self {
        match a {
            Approval::Pending => Self::Pending,
            Approval::Approved => Self::Approved,
            Approval::Denied => Self::Denied,
        }
    }
}

impl From<&Status> for ReplyStatus {
    fn from(s: &Status) -> Self {
        match s {
            Status::Running => Self::Running,
            Status::Done => Self::Done,
            Status::Failed { error } => Self::Failed {
                error: error.clone(),
            },
            Status::Interrupted => Self::Interrupted,
        }
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

impl From<&Card> for ToolCard {
    fn from(c: &Card) -> Self {
        Self {
            title: c.title.clone(),
            body: c.body.clone(),
            note: c.note.clone(),
        }
    }
}

/// A slash command the user ran, as the app shows it.
fn command(call: u64, tool: &str, input: &protocol::Json, outcome: Option<&Outcome>) -> ViewItem {
    ViewItem::Command {
        call,
        name: tool.to_owned(),
        args: Item::args(input),
        outcome: outcome.map(|o| tool_outcome(o, None)),
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
                purpose,
            } => Self::Reply {
                call: *call,
                model: model.clone(),
                text: text.clone(),
                status: status.into(),
                usage: usage.map(Into::into),
                purpose: purpose.clone(),
            },
            // The app shows a slash command apart from a tool.
            Item::Tool {
                call,
                tool,
                input,
                outcome,
                by: protocol::Audience::User,
                ..
            } => command(*call, tool, input, outcome.as_ref()),
            Item::Tool {
                call,
                tool,
                input,
                approval,
                outcome,
                card,
                ..
            } => Self::Tool {
                call: *call,
                tool: tool.clone(),
                input_json: input.to_string(),
                title: call_title(tool, input, card.as_ref()),
                approval: approval.map(Into::into),
                outcome: outcome.as_ref().map(|o| tool_outcome(o, card.as_ref())),
                card: card.as_ref().map(Into::into),
                phase: item.phase().map_or(CallPhase::Running, Into::into),
            },

            Item::Hook {
                seq,
                hook,
                call,
                tool,
                decision,
            } => Self::Hook {
                seq: *seq,
                hook: hook.clone(),
                decision_json: serde_json::to_string(decision).unwrap_or_default(),
                line: decision_line(tool, *call, decision),
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
                fired: fired.as_ref().map(|o| tool_outcome(o, None)),
            },
            Item::Environment { seq, cwd } => Self::Environment {
                seq: *seq,
                cwd: cwd.display().to_string(),
            },
        }
    }
}

fn tool_outcome(outcome: &Outcome, card: Option<&Card>) -> ToolOutcome {
    match outcome {
        Outcome::Done { output } => ToolOutcome::Done {
            output_json: output.to_string(),
            shown: output_text(card).map(|(text, note)| OutputText { text, note }),
        },
        Outcome::Failed { error } => ToolOutcome::Failed {
            error: error.clone(),
        },
        Outcome::Interrupted { .. } => ToolOutcome::Interrupted,
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
    fn of(client: &SessionClient, rev: u64) -> Self {
        let view = client.view();
        Self {
            rev,
            seq: view.seq,
            session: view.session.map(|s| s.to_string()),
            workspace: view.workspace.display().to_string(),
            items: view.items.iter().map(Into::into).collect(),
            link: client.link().into(),
            recap: view.recap.as_ref().map(Into::into),
            streaming: view.streaming.as_ref().map(|s| StreamingText {
                call: s.call,
                text: s.text.clone(),
            }),
            turn: client.turn().into(),
            title: view.title.clone(),
            state_hash: format!("{:016x}", view.state_hash()),
        }
    }
}
