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
//! whose reply is streaming, and each item how it is shown
//! ([`Presentation`]), so no rule is copied into Swift.

mod feed;
mod live;

use std::fmt;
use std::io;

use boltffi::{data, error};
use client::{End, Link, Notice, SessionClient, TurnView};
use protocol::view::{
    Buttons, Change, Glyph, Item, Part, Presented, Recap, SessionView, Shown, Status,
    mismatch_line, recap_line,
};
use protocol::{CommandInfo, Outcome, SessionSummary};

pub use feed::Reducer;
pub use live::{Client, Session, SessionWatch, find_session};

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

/// A transcript item; see [`protocol::view::Item`]. Each carries how it
/// is `shown`: the app lays that out and decides nothing else.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub enum ViewItem {
    User {
        seq: u64,
        text: String,
        shown: Presentation,
    },
    Assistant {
        seq: u64,
        text: String,
        shown: Presentation,
    },
    /// A model call and the reply it got. A side request an extension
    /// made names its `purpose`: its reply is the extension's (a title is
    /// the session's [`SessionSnapshot::title`]), not the conversation's.
    Reply {
        call: u64,
        model: String,
        text: String,
        status: ReplyStatus,
        purpose: Option<String>,
        shown: Presentation,
    },
    Tool {
        call: u64,
        tool: String,
        /// JSON.
        input_json: String,
        outcome: Option<ToolOutcome>,
        shown: Presentation,
    },
    /// A slash command and its outcome.
    Command {
        call: u64,
        name: String,
        args: String,
        outcome: Option<ToolOutcome>,
        shown: Presentation,
    },
    Hook {
        seq: u64,
        hook: String,
        /// JSON.
        decision_json: String,
        shown: Presentation,
    },
    Child {
        call: u64,
        child: String,
        input: String,
        watch: Option<u64>,
        fired: Option<ToolOutcome>,
        shown: Presentation,
    },
    Environment {
        seq: u64,
        cwd: String,
        shown: Presentation,
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

    /// How the item is shown.
    pub fn shown(&self) -> Presentation {
        match self {
            Self::User { shown, .. }
            | Self::Assistant { shown, .. }
            | Self::Reply { shown, .. }
            | Self::Tool { shown, .. }
            | Self::Command { shown, .. }
            | Self::Hook { shown, .. }
            | Self::Child { shown, .. }
            | Self::Environment { shown, .. } => shown.clone(),
        }
    }
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
    pub body: Option<ShownBody>,
    /// A reply's token counts, once it has them.
    pub usage: Option<TokenUsage>,
    /// The call whose approval this item asks for: show the buttons.
    pub approve: Option<u64>,
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

impl From<&Presented<'_>> for Presentation {
    fn from(p: &Presented<'_>) -> Self {
        let body = match &p.body {
            Shown::None => None,
            body => Some(ShownBody {
                kind: match body {
                    Shown::Diff(_) => BodyKind::Diff,
                    Shown::Json(_) => BodyKind::Json,
                    Shown::Markdown(_) => BodyKind::Markdown,
                    Shown::None | Shown::Text(_) => BodyKind::Text,
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
            body,
            usage: p.usage.map(Into::into),
            approve: match p.buttons {
                Buttons::Approve(call) => Some(call.get()),
                Buttons::None => None,
            },
        }
    }
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

/// What changed in an item; see [`protocol::view::Part`]. Each patch
/// brings how the item is shown now.
#[data]
#[derive(Clone, Debug, PartialEq)]
pub enum ItemPatch {
    /// A tool's, a command's or a child's outcome.
    Outcome {
        outcome: ToolOutcome,
        shown: Presentation,
    },
    /// A reply's text and status.
    Reply {
        text: String,
        status: ReplyStatus,
        shown: Presentation,
    },
    /// Where the call stands changed: it is asked about, runs, or was
    /// refused.
    Phase { shown: Presentation },
    /// The approval the call waited for was given, or not.
    Answer { approved: bool, shown: Presentation },
    /// A child's watch, fired or not.
    Watch { watch: Option<u64> },
    /// The item as it is now: a typed line became the slash command it
    /// ran, which then got its outcome.
    Command { item: ViewItem },
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
            Change::Updated { index: i, part } => Self::Updated {
                index: index(*i),
                patch: ItemPatch::of(&view.items[*i], *part),
            },
            Change::Delta { call, text } => Self::Delta {
                call: call.get(),
                text: text.clone(),
            },
            Change::Synced => Self::Synced {
                recap: view.recap.as_ref().map(Into::into),
            },
            Change::Snapshot => Self::Snapshot {
                items: view.items.iter().map(Into::into).collect(),
            },
            Change::Draining => Self::Draining,
            Change::Mismatch { supported } => Self::Mismatch {
                supported: *supported,
                message: mismatch_line(*supported),
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
        let shown = Presentation::from(&item.present());
        match (item, part) {
            (
                Item::Tool {
                    by: protocol::Audience::User,
                    ..
                },
                _,
            ) => Self::Command { item: item.into() },
            (Item::Child { watch, .. }, Part::Watch) => Self::Watch {
                watch: watch.map(protocol::Seq::get),
            },
            (Item::Reply { text, status, .. }, Part::Outcome) => Self::Reply {
                text: text.clone(),
                status: status.into(),
                shown,
            },
            (Item::Tool { outcome, .. }, Part::Outcome) => Self::Outcome {
                outcome: outcome
                    .as_ref()
                    .map_or(ToolOutcome::Interrupted, Into::into),
                shown,
            },
            (Item::Child { fired, .. }, Part::Outcome) => Self::Outcome {
                outcome: fired.as_ref().map_or(ToolOutcome::Interrupted, Into::into),
                shown,
            },
            (_, Part::Answer { approved }) => Self::Answer { approved, shown },
            (_, Part::Phase) => Self::Phase { shown },
            // Nothing else is ever updated.
            (item, part) => unreachable!("{part:?} of {item:?}"),
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

impl From<&Item> for ViewItem {
    fn from(item: &Item) -> Self {
        let shown = Presentation::from(&item.present());
        match item {
            Item::User { seq, text } => Self::User {
                seq: seq.get(),
                text: text.clone(),
                shown,
            },
            Item::Assistant { seq, text } => Self::Assistant {
                seq: seq.get(),
                text: text.clone(),
                shown,
            },
            Item::Reply {
                call,
                model,
                text,
                status,
                purpose,
                ..
            } => Self::Reply {
                call: call.get(),
                model: model.clone(),
                text: text.clone(),
                status: status.into(),
                purpose: purpose.clone(),
                shown,
            },
            // The app shows a slash command apart from a tool.
            Item::Tool {
                call,
                tool,
                input,
                outcome,
                by: protocol::Audience::User,
                ..
            } => Self::Command {
                call: call.get(),
                name: tool.clone(),
                args: Item::args(input),
                outcome: outcome.as_ref().map(Into::into),
                shown,
            },
            Item::Tool {
                call,
                tool,
                input,
                outcome,
                ..
            } => Self::Tool {
                call: call.get(),
                tool: tool.clone(),
                input_json: input.to_string(),
                outcome: outcome.as_ref().map(Into::into),
                shown,
            },
            Item::Hook {
                seq,
                hook,
                decision,
                ..
            } => Self::Hook {
                seq: seq.get(),
                hook: hook.clone(),
                decision_json: serde_json::to_string(decision).unwrap_or_default(),
                shown,
            },
            Item::Child {
                call,
                child,
                input,
                watch,
                fired,
            } => Self::Child {
                call: call.get(),
                child: child.to_string(),
                input: input.clone(),
                watch: watch.map(protocol::Seq::get),
                fired: fired.as_ref().map(Into::into),
                shown,
            },
            Item::Environment { seq, cwd } => Self::Environment {
                seq: seq.get(),
                cwd: cwd.display().to_string(),
                shown,
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
            Outcome::Interrupted { .. } => Self::Interrupted,
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
                call: s.call.get(),
                text: s.text.clone(),
            }),
            turn: client.turn().into(),
            title: view.side(TITLE).map(str::to_owned),
            state_hash: format!("{:016x}", view.state_hash()),
        }
    }
}
