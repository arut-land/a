//! Wire types shared by the daemon and every client, plus framing.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The protocol this build speaks best.
pub const PROTO: u32 = 1;

/// The oldest protocol this build still speaks: a peer whose range
/// overlaps `PROTO_MIN..=PROTO` is served at the highest version both speak.
pub const PROTO_MIN: u32 = 1;

const MAX_FRAME: u32 = 16 * 1024 * 1024;

mod decode;
mod json;
pub mod ledger;
pub mod view;

pub use json::Json;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Short form for display: the random tail of the `UUIDv7`.
    pub const fn short(self) -> Short {
        Short(self)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// All 32 hex digits, no dashes.
impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.simple(), f)
    }
}

/// The last 8 hex digits of a [`SessionId`].
#[derive(Clone, Copy, Debug)]
pub struct Short(SessionId);

impl fmt::Display for Short {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = Uuid::encode_buffer();
        f.write_str(&self.0.0.simple().encode_lower(&mut buf)[24..])
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub seq: u64,
    /// The lease its writer held: a store refuses the appends of a writer
    /// whose lease was superseded.
    pub epoch: u32,
    /// Reserved for entries addressed across sessions, as forks and
    /// migration will need: a global id, and the entry this one follows
    /// when that is not the one before it. They share the entry's object
    /// with its body's fields, so no body has fields of these names (a tool
    /// call's `id` is the model's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Uuid>,
    #[serde(flatten)]
    pub body: Body,
}

impl Entry {
    pub const fn new(seq: u64, epoch: u32, body: Body) -> Self {
        Self {
            seq,
            epoch,
            uuid: None,
            parent: None,
            body,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// The user's message, starting a turn.
    User {
        text: String,
    },
    /// Input taken in, logged on arrival so that it survives a crash and
    /// every client sees it waiting. Queued input is taken in order: each
    /// `user` or `command` entry takes the oldest still waiting, and a
    /// `steer` entry the one it names. A `steer` goes to the running turn
    /// at its next step boundary rather than after it.
    Queued {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        steer: bool,
    },
    /// Steering input `queued`, given to the running turn: the model reads
    /// it as the user's before its next reply.
    Steer {
        text: String,
        queued: u64,
    },
    Assistant {
        text: String,
    },
    /// The committed intent to run a tool. Its seq is the call's id and
    /// idempotency key; `id` is the model's own id for the call, under which
    /// its result goes back.
    ToolCall {
        tool: String,
        input: Json,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// How clients show the call, as the tool presents it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        card: Option<Card>,
    },
    /// The process group running a call, so recovery can kill it. Only
    /// meaningful within the same `boot`: pgids are reused across reboots.
    /// A `background` group outlives its call: the session owns it until
    /// the session ends.
    ToolProcess {
        call: u64,
        pgid: u32,
        boot: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        background: bool,
    },
    /// Output tool call `call` produced since its last progress entry,
    /// committed as it runs: what an interrupted call reports, and what
    /// clients show meanwhile.
    Progress {
        call: u64,
        text: String,
    },
    ToolResult {
        call: u64,
        outcome: Outcome,
        /// How clients show the outcome, as the tool presents it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        card: Option<Card>,
    },
    /// The committed intent to ask a model for a reply.
    ModelCall {
        model: String,
    },
    /// The reply; `Done` carries the whole `llm::Response`, or
    /// `{"text": ...}` in logs older than that.
    ModelResult {
        call: u64,
        outcome: Outcome,
    },
    /// A hook changed what happens at a decision point.
    Hook {
        hook: String,
        decision: Decision,
    },
    /// Tool call `call` waits for the user's approval. Until a
    /// [`Decision::Approval`] for it is logged it is in the session's inbox:
    /// delivered to every client, and again to each that attaches.
    Ask {
        call: u64,
    },
    /// The committed intent to start session `child` with `input`. Its seq
    /// is the call that owns the child; the child's id is chosen before the
    /// commit, so recovery finds the child instead of making another.
    Link {
        child: SessionId,
        input: String,
    },
    /// A durable watch on `child`: when it settles, its result comes back
    /// as a `fired` under this entry's seq, which is the idempotency key.
    Watch {
        child: SessionId,
    },
    /// Watch `watch` fired, with its child's result. Logged at most once
    /// per watch.
    Fired {
        watch: u64,
        outcome: Outcome,
    },
    /// The environment the session's commands run in from here on, as the
    /// attaching client captured it, secrets redacted.
    Environment(Environment),
    /// The extensions the session runs with from here on, in install order,
    /// logged when they differ from those its header or an earlier entry
    /// records.
    Extensions {
        installed: Vec<Installed>,
    },
    /// A change to extension `ext`'s durable state `state`: folding these
    /// in log order rebuilds it.
    Ext {
        ext: String,
        state: String,
        data: Json,
    },
    /// Facts about where the session runs (its directory, the workspace's
    /// agent notes), which the model reads as a user message ahead of the
    /// conversation. Logged once, before the first model call, so the
    /// system prompt stays the same across sessions and cacheable.
    Context {
        text: String,
    },
    /// The committed intent to run slash command `/name args`, which takes
    /// no model turn. Its seq is the command's id.
    Command {
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        args: String,
    },
    CommandResult {
        call: u64,
        outcome: Outcome,
    },
    /// The user interrupted the turn: what the agent asked for and had not
    /// started is dropped.
    Interrupt,
    /// The session's own settings from here on, over every config layer;
    /// each entry replaces the one before.
    Settings(Overrides),
}

/// Settings a session sets for itself; those unset come from the config
/// layers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overrides {
    /// The model, as `host:model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// How a client shows a tool call or its outcome: pure data the tool
/// derives from its input and output, so every client renders any tool
/// alike without knowing it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    /// One line: what the call does, such as `$ ls` or `edit src/main.rs`.
    pub title: String,
    /// Preformatted text under it, such as output or a diff.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// A short status, such as `exit 1`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// An extension a session ran with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub name: String,
    pub version: u32,
}

/// A client's shell environment: what a command the agent runs for it sees.
/// Commands run with exactly `vars` in `cwd`, never with the daemon's own.
///
/// Secret values live only in memory: the logged form names them in
/// `redacted` instead, so after a daemon restart commands run without them
/// until a client attaches or refreshes again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    pub cwd: PathBuf,
    pub vars: BTreeMap<String, String>,
    /// Secrets whose values were left out of `vars`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted: Vec<String>,
    /// Whether the client ran at a terminal.
    #[serde(default)]
    pub tty: bool,
}

/// Bookkeeping of the client's shell, not meant for the commands run for
/// it: never captured.
const DENIED: &[&str] = &["_", "OLDPWD", "PWD", "SHLVL"];

/// Never secret, whatever the patterns below say.
const PUBLIC: &[&str] = &["SSH_AUTH_SOCK", "GPG_AGENT_INFO"];

/// A name containing any of these, in any case, holds a secret…
const SECRET_WORDS: &[&str] = &[
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "TOKEN",
    "CREDENTIAL",
    "API_KEY",
    "PRIVATE_KEY",
];

/// …as does one ending in one of these.
const SECRET_SUFFIXES: &[&str] = &["_KEY", "_PASS", "_PAT", "_DSN", "_COOKIE"];

impl Environment {
    /// Captures `vars`, leaving out denied names and anything not UTF-8.
    pub fn new(
        cwd: PathBuf,
        vars: impl IntoIterator<Item = (OsString, OsString)>,
        tty: bool,
    ) -> Self {
        // Inserted one by one: collecting would sort, in code of its own.
        let mut map = BTreeMap::new();
        for (k, v) in vars {
            if let (Ok(k), Ok(v)) = (k.into_string(), v.into_string())
                && !DENIED.contains(&k.as_str())
            {
                map.insert(k, v);
            }
        }
        Self {
            cwd,
            vars: map,
            redacted: Vec::new(),
            tty,
        }
    }

    /// Whether variable `name` holds a secret, whose value is never logged.
    pub fn is_secret(name: &str) -> bool {
        let name = name.to_ascii_uppercase();
        !PUBLIC.contains(&name.as_str())
            && (SECRET_WORDS.iter().any(|w| name.contains(w))
                || SECRET_SUFFIXES.iter().any(|s| name.ends_with(s)))
    }

    /// This environment as it may be logged: secret values dropped, their
    /// names kept in `redacted`.
    #[must_use]
    pub fn redacted(&self) -> Self {
        let mut redacted = self.redacted.clone();
        let mut vars = BTreeMap::new();
        for (k, v) in &self.vars {
            if Self::is_secret(k) {
                redacted.push(k.clone());
            } else {
                vars.insert(k.clone(), v.clone());
            }
        }
        Self {
            cwd: self.cwd.clone(),
            vars,
            redacted,
            tty: self.tty,
        }
    }
}

/// What a hook decided, by decision point. Only decisions that change
/// something are logged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// Externally tagged: an internally tagged enum inside the internally tagged
// `Body` costs a buffered deserializer per variant.
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// The request of model call `call` was replaced before it was sent.
    PrepareStep { call: u64 },
    /// A tool call's input was replaced by `input`.
    PreTool { tool: String, input: Json },
    /// A tool call was blocked before it ran.
    Block { tool: String, reason: String },
    /// The most restrictive permission the hooks gave a tool call.
    Permission {
        tool: String,
        permission: Permission,
    },
    /// The answer to a permission `ask`; `call` is set when the user gave
    /// it, to the call's [`Body::Ask`].
    Approval {
        tool: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<u64>,
    },
    /// Call `call`'s outcome was replaced; `original` is what it replaced.
    PostTool { call: u64, original: Outcome },
    /// The agent stopped and was given `text` to continue with.
    OnStop { text: String },
    /// The user's input was rewritten: the user entry that follows holds
    /// the new text, `original` what it replaced.
    Input { original: String },
    /// Model call `call` failed with `error` and is sent again, for the
    /// `attempt`th time, with the request as the hook left it.
    Retry {
        call: u64,
        attempt: u32,
        error: String,
    },
}

/// Whether a tool call may run, from least to most restrictive: merging
/// takes the maximum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    #[default]
    Allow,
    /// Runs once someone approves it.
    Ask,
    /// Decided outside this run, later.
    Defer,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Done {
        output: Json,
    },
    Failed {
        error: String,
    },
    /// The call stopped before it finished: the user interrupted the turn,
    /// or the daemon stopped and the tool is not safe to rerun. `output` is
    /// what it had produced by then.
    Interrupted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    New,
    Latest {
        any_workspace: bool,
    },
    Id(SessionId),
    /// The one session whose id starts with this, in full or short form.
    /// Only sent with [`Cap::Sessions`].
    Prefix(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub workspace: PathBuf,
    pub entries: u64,
    pub clients: u32,
    #[serde(default)]
    pub activity: Activity,
    #[serde(default)]
    pub residency: Residency,
}

/// What a session is doing. A daemon older than this field reports every
/// session `idle`; a newer one's states this build does not know read as
/// `other`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    #[default]
    Idle,
    /// Working on queued input, a command, or a watch not yet fired.
    Running,
    /// Requires action: a tool call waits for the user's approval.
    Approval,
    /// Requires action: a dialog waits for the user's input.
    Input,
    /// Requires action: a tool waits on something outside the session,
    /// such as a sign-in.
    Tool,
    /// Requires action: a call routed to a client that has gone waits for
    /// one to come back.
    Client,
    /// Parked on purpose; nothing runs until it is resumed.
    Suspended,
    /// Finished for good, such as a child whose result is in.
    Done,
    /// Ended by an error it cannot recover from.
    Failed,
    #[serde(other)]
    Other,
}

impl Activity {
    /// Whether the session waits on someone before it can go on.
    pub const fn requires_action(self) -> bool {
        matches!(
            self,
            Self::Approval | Self::Input | Self::Tool | Self::Client
        )
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Approval => "approval",
            Self::Input => "input",
            Self::Tool => "tool",
            Self::Client => "client",
            Self::Suspended => "suspended",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Other => "other",
        }
    }
}

/// Where a session's state is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Residency {
    /// In memory, its actor running.
    Loaded,
    /// Only in its log; it loads on the next attach.
    #[default]
    Hibernated,
    /// Moving to another host.
    Migrating,
    /// Run by another host and followed from here.
    Remote,
    #[serde(other)]
    Other,
}

impl Residency {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Hibernated => "hibernated",
            Self::Migrating => "migrating",
            Self::Remote => "remote",
            Self::Other => "other",
        }
    }
}

/// What happened in a session, for a client that attaches: derived from the
/// log alone, so every client gets the same one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recap {
    /// The user's last message, shortened.
    pub prompt: Option<String>,
    /// The tools called since, in order.
    pub tools: Vec<String>,
    /// Calls since that failed or were interrupted.
    pub failed: u32,
    /// Calls since with no outcome yet.
    pub running: u32,
    /// The agent's last reply since, shortened.
    pub reply: Option<String>,
    /// Calls waiting for the user's approval.
    pub asks: u32,
}

/// A feature beyond the base protocol. A client offers the ones it handles
/// in its hello and the welcome names those the daemon takes up; what a
/// capability brings is used only on connections that took it up, so adding
/// one needs no protocol bump and leaves old peers as they were. A
/// capability whose shape changes gets a new name (`snapshot.2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    /// The daemon may send a [`DaemonMsg::Snapshot`] in place of a long
    /// replay, or to a client too slow to keep up.
    Snapshot,
    /// The client numbers its mutations (`client_seq`); the daemon answers
    /// each with [`DaemonMsg::Ack`] or [`DaemonMsg::Rejected`] in place of
    /// [`DaemonMsg::Error`], and a repeat the way it answered the first.
    Ack,
    /// The daemon takes [`ClientMsg::Declare`]. (The `decl` an attach
    /// carries needs no capability: a daemon older than it ignores it.)
    Declare,
    /// The daemon takes [`ClientMsg::Steer`]; the first ones drop a
    /// connection that sends it.
    Steer,
    /// The daemon takes [`ClientMsg::Settings`]: it is the one writer of
    /// `$A_HOME/config.toml`.
    Settings,
    /// The daemon takes [`ClientMsg::WatchSessions`] and attaches to a
    /// [`Target::Prefix`].
    Sessions,
    /// The daemon takes [`ClientMsg::Status`] and [`ClientMsg::Shutdown`].
    Daemon,
}

impl Cap {
    /// Every capability this build knows.
    pub const ALL: [Self; 7] = [
        Self::Snapshot,
        Self::Ack,
        Self::Declare,
        Self::Steer,
        Self::Settings,
        Self::Sessions,
        Self::Daemon,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Ack => "ack",
            Self::Declare => "declare",
            Self::Steer => "steer",
            Self::Settings => "settings",
            Self::Sessions => "sessions",
            Self::Daemon => "daemon",
        }
    }

    const fn bit(self) -> u32 {
        1 << self as u32
    }
}

/// A set of [`Cap`]s. On the wire, a list of names; names this build does
/// not know are dropped, since it could not use them anyway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps(u32);

impl Caps {
    pub const NONE: Self = Self(0);

    /// Everything this build handles.
    pub const ALL: Self = {
        let mut bits = 0;
        let mut i = 0;
        while i < Cap::ALL.len() {
            bits |= Cap::ALL[i].bit();
            i += 1;
        }
        Self(bits)
    };

    pub const fn has(self, cap: Cap) -> bool {
        self.0 & cap.bit() != 0
    }

    // A reference, as `skip_serializing_if` passes it.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Cap> {
        Cap::ALL.into_iter().filter(move |&c| self.has(c))
    }
}

/// The capabilities both sides take up.
impl std::ops::BitAnd for Caps {
    type Output = Self;

    fn bitand(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl FromIterator<Cap> for Caps {
    fn from_iter<I: IntoIterator<Item = Cap>>(caps: I) -> Self {
        Self(caps.into_iter().fold(0, |bits, c| bits | c.bit()))
    }
}

impl Serialize for Caps {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.iter().map(Cap::name))
    }
}

impl<'de> Deserialize<'de> for Caps {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let names = Vec::<String>::deserialize(d)?;
        Ok(Cap::ALL
            .into_iter()
            .filter(|c| names.iter().any(|n| n == c.name()))
            .collect())
    }
}

/// The defaults in `$A_HOME/config.toml` a settings pane edits: what a
/// model call uses when its session does not say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeSettings {
    /// `host[@protocol]:model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Provider variables, such as `OPENAI_API_KEY`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// A change to the [`HomeSettings`]: only what it names changes, so two
/// clients saving at once never undo each other's change.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsChange {
    /// The new model; empty removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The new agent; empty removes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Variables to set, or with `null`, remove.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, Option<String>>,
}

/// The protocol versions a peer speaks, `min..=max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Versions {
    pub min: u32,
    pub max: u32,
}

impl Versions {
    /// This build's.
    pub const OURS: Self = Self {
        min: PROTO_MIN,
        max: PROTO,
    };

    /// `min..=max`, or just `max` without a `min`.
    pub fn new(min: Option<u32>, max: u32) -> Self {
        Self {
            min: min.unwrap_or(max).min(max),
            max,
        }
    }

    /// The highest version in both ranges.
    pub fn common(self, other: Self) -> Option<u32> {
        let max = self.max.min(other.max);
        (self.min.max(other.min) <= max).then_some(max)
    }

    /// `min` as a hello or mismatch carries it: left out when it is `max`.
    pub fn min_field(self) -> Option<u32> {
        (self.min < self.max).then_some(self.min)
    }
}

/// Just the one version.
impl From<u32> for Versions {
    fn from(proto: u32) -> Self {
        Self {
            min: proto,
            max: proto,
        }
    }
}

impl fmt::Display for Versions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.min == self.max {
            write!(f, "protocol {}", self.max)
        } else {
            write!(f, "protocols {}-{}", self.min, self.max)
        }
    }
}

/// What a client does for a session beyond showing it: declared on attach
/// and changed live with [`ClientMsg::Declare`] (a phone going to the
/// background, say). Calls the session routes to clients go to those that
/// declared the tool, and UI contributions to those that render the site.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientDecl {
    #[serde(default, skip_serializing_if = "Role::is_owner")]
    pub role: Role,
    /// Tools this client runs itself, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// UI sites it renders, such as `dialog` or `status`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<String>,
}

impl ClientDecl {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// What a client may do to a session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Owner,
    Collaborator,
    /// Read-only: its mutations are rejected. A role this build does not
    /// know counts as this one.
    #[serde(other)]
    Viewer,
}

impl Role {
    // A reference, as `skip_serializing_if` passes it.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    const fn is_owner(&self) -> bool {
        matches!(self, Self::Owner)
    }
}

/// A slash command the daemon runs: input `/name args`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
}

/// Who caused something: connection `client`'s mutation `client_seq`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub client: u32,
    pub client_seq: u64,
}

/// Why the daemon refused a mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    /// No session is attached on this connection.
    NotAttached,
    /// The daemon is restarting for an upgrade: send it again once
    /// reconnected.
    Draining,
    /// A hook refused it.
    Refused,
    /// The call is not waiting for approval.
    NotWaiting,
    /// This client attached as a viewer.
    ReadOnly,
    /// A repeat of a mutation too old for its answer to be kept.
    Stale,
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// `build` is the client's staged build id (`<version>+<hash>`); it
    /// speaks protocols `min..=proto`, or just `proto` without `min`, with
    /// capabilities `caps`. Its first messages (attach, input) may follow
    /// right behind, without waiting for the answer: unless that is a
    /// welcome, the daemon acts on none of them.
    Hello {
        proto: u32,
        build: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u32>,
        #[serde(default, skip_serializing_if = "Caps::is_empty")]
        caps: Caps,
    },
    /// `env` is the client's, for the commands the session runs; without
    /// one the session keeps the environment it had. `workspace` is the
    /// directory the client is in: the daemon takes the git working tree it
    /// is in (with `env`'s git variables) as the workspace. (Daemons before
    /// that take it as it is.)
    Attach {
        target: Target,
        workspace: PathBuf,
        since: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<Environment>,
        #[serde(default, skip_serializing_if = "ClientDecl::is_default")]
        decl: ClientDecl,
    },
    /// Replaces this client's declaration. Only sent with [`Cap::Declare`].
    Declare(ClientDecl),
    /// Replaces the attached session's environment (`/env refresh`).
    Environment(Environment),
    Input {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_seq: Option<u64>,
    },
    /// Input for the running turn, given to it at its next step boundary;
    /// with no turn running it is plain input. Only sent with
    /// [`Cap::Steer`].
    Steer {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_seq: Option<u64>,
    },
    List,
    Detach,
    /// Asks for [`DaemonMsg::Sessions`] now and whenever a session's
    /// summary changes, for as long as the connection lasts. Only sent with
    /// [`Cap::Sessions`].
    WatchSessions,
    /// Asks for [`DaemonMsg::Status`]. Only sent with [`Cap::Daemon`].
    Status,
    /// Stops the daemon, as an idle one stops; the connection closes as it
    /// exits. Only sent with [`Cap::Daemon`].
    Shutdown,
    /// Asks for the [`HomeSettings`], after making `change` to them if
    /// there is one; answered with [`DaemonMsg::Settings`]. Only sent with
    /// [`Cap::Settings`].
    Settings {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change: Option<SettingsChange>,
    },
    /// Answers the [`DaemonMsg::Ask`] for tool call `call`.
    Approve {
        call: u64,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_seq: Option<u64>,
    },
    /// Stops the attached session's turn: its running model or tool call
    /// and, bottom-up, the turns of the sessions it started.
    Interrupt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_seq: Option<u64>,
    },
}

impl ClientMsg {
    pub const INTERRUPT: Self = Self::Interrupt { client_seq: None };

    pub fn input(text: impl Into<String>) -> Self {
        Self::Input {
            text: text.into(),
            client_seq: None,
        }
    }

    pub const fn approve(call: u64, approved: bool) -> Self {
        Self::Approve {
            call,
            approved,
            client_seq: None,
        }
    }

    /// The number of a numbered mutation.
    pub const fn client_seq(&self) -> Option<u64> {
        match self {
            Self::Input { client_seq, .. }
            | Self::Steer { client_seq, .. }
            | Self::Approve { client_seq, .. }
            | Self::Interrupt { client_seq } => *client_seq,
            _ => None,
        }
    }

    /// The idempotency key of a mutation (a message that changes the
    /// session), which the client numbers; `None` for any other message.
    pub const fn client_seq_mut(&mut self) -> Option<&mut Option<u64>> {
        match self {
            Self::Input { client_seq, .. }
            | Self::Steer { client_seq, .. }
            | Self::Approve { client_seq, .. }
            | Self::Interrupt { client_seq } => Some(client_seq),
            Self::Hello { .. }
            | Self::Attach { .. }
            | Self::Declare(_)
            | Self::Environment(_)
            | Self::List
            | Self::Detach
            | Self::WatchSessions
            | Self::Status
            | Self::Shutdown
            | Self::Settings { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DaemonMsg {
    /// The hello is accepted at protocol `proto`. `caps` are the client's
    /// capabilities the daemon takes up, and `client` this connection's id,
    /// as [`Origin`]s name it.
    Welcome {
        proto: u32,
        build: String,
        #[serde(default, skip_serializing_if = "Caps::is_empty")]
        caps: Caps,
        #[serde(default, skip_serializing_if = "is_zero")]
        client: u32,
        /// The slash commands the daemon runs, for help and completion;
        /// none from a daemon older than them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        commands: Vec<CommandInfo>,
    },
    /// The daemon speaks protocols `min..=supported` (just `supported`
    /// without `min`), none of the client's.
    Mismatch {
        supported: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u32>,
    },
    /// A newer build was staged: the daemon finishes in-flight work, exits,
    /// and closes every connection. Clients then reconnect, which starts the
    /// new build, and re-attach to replay what they missed. Sent as the reply
    /// to a hello, or to connections already open when the drain began.
    Draining,
    Attached {
        session: SessionId,
        workspace: PathBuf,
    },
    Entry(Entry),
    /// Live text of an unfinished model call. Never logged: the committed
    /// `model_result` is the fact.
    Delta {
        call: u64,
        text: String,
    },
    /// Replay is complete; everything after this is live.
    Synced {
        seq: u64,
    },
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    /// The answer to [`ClientMsg::Status`].
    Status {
        pid: u32,
        build: String,
        /// Seconds since the Unix epoch at start.
        started: u64,
        /// Open connections, the asking one included.
        clients: u32,
        /// Sessions the daemon knows, loaded or not.
        known: u32,
    },
    /// The answer to [`ClientMsg::Settings`]: the settings as they are now,
    /// and why its change was not made, if it was not.
    Settings {
        settings: HomeSettings,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Sent after `Synced` when the replay held entries.
    Recap(Recap),
    /// Tool call `call` waits for approval: sent when it is asked, and
    /// after `Synced` for every call still waiting.
    Ask {
        call: u64,
        tool: String,
        input: Json,
    },
    Error {
        message: String,
    },
    /// The whole log-derived view at `view.seq`, in place of the replay up
    /// to it: for an attach from far behind, or a client that fell too far
    /// behind live. Folding it and what follows equals folding the entries
    /// it stands for. Only sent with [`Cap::Snapshot`].
    Snapshot(Box<view::SessionView>),
    /// Mutation `origin` was taken; what it causes follows as entries. Only
    /// sent with [`Cap::Ack`].
    Ack {
        origin: Origin,
    },
    /// Mutation `origin` was refused. Only sent with [`Cap::Ack`].
    Rejected {
        origin: Origin,
        reason: Rejection,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        message: String,
    },
    /// A message, or an entry, of a kind this build does not know: a newer
    /// daemon's. Never sent; decoding yields it so a client skips what it
    /// cannot show instead of dropping the connection. `seq` is the
    /// entry's.
    #[serde(skip)]
    Unknown {
        seq: Option<u64>,
    },
}

// A reference, as `skip_serializing_if` passes it.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// How a message stands to the session's log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Derived from the log, so a replay from any seq gives it again: a
    /// client that falls behind is caught up rather than sent every such
    /// message it missed.
    Durable,
    /// Never logged: live progress a later durable fact supersedes, which
    /// may be dropped for a client that is behind.
    Ephemeral,
    /// About the connection rather than the session: always delivered, in
    /// order.
    Control,
}

impl Body {
    /// Every entry kind is durable so far (bash progress included: it is
    /// the partial output recovery reports). An ephemeral kind (usage ticks,
    /// presence) would go to live clients and never be replayed. The match
    /// fails to compile when a kind is added, so that it is classified.
    pub const fn class(&self) -> Class {
        match self {
            Self::User { .. }
            | Self::Assistant { .. }
            | Self::ToolCall { .. }
            | Self::ToolProcess { .. }
            | Self::ToolResult { .. }
            | Self::ModelCall { .. }
            | Self::ModelResult { .. }
            | Self::Hook { .. }
            | Self::Ask { .. }
            | Self::Link { .. }
            | Self::Watch { .. }
            | Self::Fired { .. }
            | Self::Environment(_)
            | Self::Extensions { .. }
            | Self::Ext { .. }
            | Self::Context { .. }
            | Self::Command { .. }
            | Self::CommandResult { .. }
            | Self::Queued { .. }
            | Self::Steer { .. }
            | Self::Progress { .. }
            | Self::Interrupt
            | Self::Settings(_) => Class::Durable,
        }
    }
}

impl DaemonMsg {
    pub const fn class(&self) -> Class {
        match self {
            Self::Entry(e) => e.body.class(),
            Self::Attached { .. }
            | Self::Synced { .. }
            | Self::Recap(_)
            | Self::Ask { .. }
            | Self::Snapshot(_) => Class::Durable,
            Self::Delta { .. } => Class::Ephemeral,
            Self::Welcome { .. }
            | Self::Mismatch { .. }
            | Self::Draining
            | Self::Sessions { .. }
            | Self::Settings { .. }
            | Self::Status { .. }
            | Self::Error { .. }
            | Self::Ack { .. }
            | Self::Rejected { .. }
            | Self::Unknown { .. } => Class::Control,
        }
    }
}

/// Writes one frame: a little-endian u32 length followed by a JSON body,
/// in one write.
pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let mut bytes = Frame::start();
    serde_json::to_writer(&mut bytes, msg)?;
    prefix(&mut bytes)?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Fills in the length of the frame `bytes` holds after its first four.
fn prefix(bytes: &mut [u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len() - 4)
        .ok()
        .filter(|&n| n <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    bytes[..4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// A frame encoded once, length prefix included, for any number of peers:
/// cloning it shares the bytes.
#[derive(Clone, Debug)]
pub struct Frame {
    bytes: Arc<[u8]>,
    /// The entry's seq, for an entry.
    pub seq: Option<u64>,
    pub class: Class,
}

impl Frame {
    pub fn new(msg: &DaemonMsg) -> io::Result<Self> {
        let mut bytes = Self::start();
        serde_json::to_writer(&mut bytes, msg)?;
        Self::finish(bytes, None, msg.class())
    }

    /// The frame of `DaemonMsg::Entry(entry.clone())`, without the clone:
    /// the entry's own JSON with the tag spliced in first, as serde puts it.
    pub fn entry(entry: &Entry) -> io::Result<Self> {
        let mut bytes = Self::start();
        bytes.extend_from_slice(br#"{"t":"entry""#);
        let at = bytes.len();
        entry.encode(&mut bytes)?;
        bytes[at] = b',';
        Self::finish(bytes, Some(entry.seq), entry.body.class())
    }

    /// Room for the length, and as much again as `serde_json::to_vec`
    /// starts with.
    fn start() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(132);
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    fn finish(mut bytes: Vec<u8>, seq: Option<u64>, class: Class) -> io::Result<Self> {
        prefix(&mut bytes)?;
        Ok(Self {
            bytes: Arc::from(bytes),
            seq,
            class,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Entry `seq`'s frame from the line a log stores for it, newline
    /// excluded: the bytes encoded at commit, tagged, not encoded again.
    pub fn from_line(seq: u64, line: &[u8]) -> io::Result<Self> {
        const TAG: &[u8] = br#"{"t":"entry","#;
        let fields = line
            .strip_prefix(b"{")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an entry's line"))?;
        let mut bytes = Vec::with_capacity(4 + TAG.len() + fields.len());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(TAG);
        bytes.extend_from_slice(fields);
        // Every entry kind is durable so far; see `Body::class`.
        Self::finish(bytes, Some(seq), Class::Durable)
    }
}

impl Entry {
    /// Appends the entry's JSON object to `out`. Encoded here, once, for
    /// the reason [`Decode`] gives.
    pub fn encode(&self, out: &mut Vec<u8>) -> serde_json::Result<()> {
        serde_json::to_writer(out, self)
    }
}

/// A record decoded from JSON here, once, rather than in every crate that
/// reads one: release builds do not share generic instantiations across
/// crates, and these deserializers are large.
pub trait Decode: Sized {
    fn from_json(bytes: &[u8]) -> serde_json::Result<Self>;
}

/// Reads one frame; `Ok(None)` means the peer closed the stream cleanly.
pub fn read_frame<T: Decode>(r: &mut impl Read) -> io::Result<Option<T>> {
    match read_body(r)? {
        Some(body) => Ok(Some(T::from_json(&body)?)),
        None => Ok(None),
    }
}

/// The body of the next frame, read once for every type and stream.
fn read_body(r: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    // Grown as the bytes arrive, so a length alone allocates no more than
    // the peer goes on to send.
    let mut body = Vec::with_capacity((len as usize).min(64 * 1024));
    r.take(u64::from(len)).read_to_end(&mut body)?;
    if body.len() < len as usize {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::decode::{known_kind, known_msg};
    use super::*;

    #[test]
    fn a_shared_frame_is_the_frame_written() {
        let entry = Entry::new(7, 0, Body::Assistant { text: "hi".into() });
        let mut written = Vec::new();
        write_frame(&mut written, &DaemonMsg::Entry(entry.clone())).unwrap();
        assert_eq!(Frame::entry(&entry).unwrap().as_bytes(), written);
        let msg = DaemonMsg::Synced { seq: 7 };
        written.clear();
        write_frame(&mut written, &msg).unwrap();
        assert_eq!(Frame::new(&msg).unwrap().as_bytes(), written);
    }

    /// A length the peer never sends the bytes for costs it nothing but an
    /// error: the body is read as it comes, not allocated up front.
    #[test]
    fn a_frame_cut_short_is_an_error_whatever_length_it_claims() {
        /// Counts the bytes asked for: what reading the frame would buffer.
        struct Counted<'a>(&'a [u8], usize);
        impl Read for Counted<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.1 = self.1.max(buf.len());
                self.0.read(buf)
            }
        }
        let mut claim = MAX_FRAME.to_le_bytes().to_vec();
        claim.extend_from_slice(br#"{"t":"list"}"#);
        let mut r = Counted(&claim, 0);
        let err = read_frame::<ClientMsg>(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(r.1 <= 128 * 1024, "asked for {} bytes at once", r.1);
    }

    #[test]
    fn frames_round_trip() {
        let msg = ClientMsg::input("hi");
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        write_frame(&mut buf, &ClientMsg::Detach).unwrap();
        let mut r = buf.as_slice();
        assert!(
            matches!(read_frame(&mut r).unwrap(), Some(ClientMsg::Input { text, .. }) if text == "hi")
        );
        assert!(matches!(
            read_frame(&mut r).unwrap(),
            Some(ClientMsg::Detach)
        ));
        assert!(read_frame::<ClientMsg>(&mut r).unwrap().is_none());
    }

    #[test]
    fn session_ids_display_whole_and_short() {
        let id = SessionId(Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef));
        assert_eq!(id.to_string(), "0123456789abcdef0123456789abcdef");
        assert_eq!(id.short().to_string(), "89abcdef");
    }

    #[test]
    fn draining_is_a_bare_tag() {
        assert_eq!(
            serde_json::to_string(&DaemonMsg::Draining).unwrap(),
            r#"{"t":"draining"}"#
        );
    }

    #[test]
    fn entry_wire_shape_is_flat() {
        let e = Entry::new(1, 0, Body::User { text: "hi".into() });
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"seq":1,"epoch":0,"kind":"user","text":"hi"}"#
        );
    }

    #[test]
    fn tool_calls_without_a_model_id_keep_their_shape() {
        let old = r#"{"seq":2,"epoch":0,"kind":"tool_call","tool":"sleep","input":{"seconds":1}}"#;
        let e: Entry = serde_json::from_str(old).unwrap();
        assert!(matches!(&e.body, Body::ToolCall { id: None, .. }));
        assert_eq!(serde_json::to_string(&e).unwrap(), old);
    }

    #[test]
    fn secrets_are_redacted_by_name() {
        let vars = [
            ("PATH", "/bin"),
            ("OPENAI_API_KEY", "sk-1"),
            ("GITHUB_TOKEN", "ghp-1"),
            ("DB_PASSWORD", "hunter2"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("PWD", "/x"),
        ]
        .map(|(k, v)| (k.into(), v.into()));
        let env = Environment::new("/w".into(), vars, true);
        assert!(!env.vars.contains_key("PWD"));
        let logged = env.redacted();
        assert_eq!(
            logged.vars.keys().collect::<Vec<_>>(),
            ["PATH", "SSH_AUTH_SOCK"]
        );
        assert_eq!(
            logged.redacted.iter().collect::<Vec<_>>(),
            ["DB_PASSWORD", "GITHUB_TOKEN", "OPENAI_API_KEY"]
        );
        assert_eq!(logged.redacted(), logged);
        let json = serde_json::to_string(&Body::Environment(logged)).unwrap();
        assert!(
            json.starts_with(r#"{"kind":"environment","cwd":"/w","#),
            "{json}"
        );
        for secret in ["sk-1", "ghp-1", "hunter2"] {
            assert!(!json.contains(secret), "{json}");
        }
    }

    #[test]
    fn hook_decisions_nest_under_their_point_name() {
        let body = Body::Hook {
            hook: "policy".into(),
            decision: Decision::Permission {
                tool: "run_command".into(),
                permission: Permission::Deny,
            },
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({
                "kind": "hook",
                "hook": "policy",
                "decision": { "permission": { "tool": "run_command", "permission": "deny" } },
            })
        );
    }

    #[test]
    fn approvals_by_a_hook_keep_their_shape_and_the_users_name_the_call() {
        let by_hook = r#"{"seq":2,"epoch":0,"kind":"hook","hook":"h","decision":{"approval":{"tool":"t","approved":true}}}"#;
        let e: Entry = serde_json::from_str(by_hook).unwrap();
        assert_eq!(serde_json::to_string(&e).unwrap(), by_hook);
        let by_user = Body::Hook {
            hook: "user".into(),
            decision: Decision::Approval {
                tool: "t".into(),
                approved: false,
                call: Some(3),
            },
        };
        assert_eq!(
            serde_json::to_value(&by_user).unwrap()["decision"]["approval"]["call"],
            3
        );
    }

    #[test]
    fn unknown_kinds_are_skipped_and_malformed_known_ones_are_fatal() {
        let msg = |json: &str| DaemonMsg::from_json(json.as_bytes());
        assert!(matches!(
            msg(r#"{"t":"presence","who":1}"#),
            Ok(DaemonMsg::Unknown { seq: None })
        ));
        assert!(matches!(
            msg(r#"{"t":"entry","seq":4,"epoch":0,"kind":"later","x":1}"#),
            Ok(DaemonMsg::Unknown { seq: Some(4) })
        ));
        assert!(msg(r#"{"t":"entry","seq":4,"epoch":0,"kind":"user"}"#).is_err());
        assert!(msg(r#"{"t":"synced"}"#).is_err());
        assert!(msg(r#"{"seq":1}"#).is_err());
    }

    /// The matches fail to compile when a variant is added, so that it is
    /// added to the known tags as well.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_tag_this_build_sends_is_known() {
        let child = SessionId(Uuid::from_u128(1));
        let bodies = [
            Body::User { text: "u".into() },
            Body::Queued {
                text: "q".into(),
                steer: true,
            },
            Body::Steer {
                text: "s".into(),
                queued: 1,
            },
            Body::Assistant { text: "a".into() },
            Body::ToolCall {
                tool: "t".into(),
                input: Json::null(),
                id: None,
                card: None,
            },
            Body::ToolProcess {
                call: 1,
                pgid: 1,
                boot: "b".into(),
                background: false,
            },
            Body::Progress {
                call: 1,
                text: "p".into(),
            },
            Body::ToolResult {
                call: 1,
                outcome: Outcome::Interrupted { output: None },
                card: None,
            },
            Body::ModelCall { model: "m".into() },
            Body::ModelResult {
                call: 1,
                outcome: Outcome::Interrupted { output: None },
            },
            Body::Hook {
                hook: "h".into(),
                decision: Decision::OnStop { text: "t".into() },
            },
            Body::Ask { call: 1 },
            Body::Link {
                child,
                input: "i".into(),
            },
            Body::Watch { child },
            Body::Fired {
                watch: 1,
                outcome: Outcome::Interrupted { output: None },
            },
            Body::Environment(Environment::default()),
            Body::Extensions {
                installed: Vec::new(),
            },
            Body::Ext {
                ext: "e".into(),
                state: "s".into(),
                data: Json::null(),
            },
            Body::Context { text: "c".into() },
            Body::Command {
                name: "n".into(),
                args: String::new(),
            },
            Body::CommandResult {
                call: 1,
                outcome: Outcome::Interrupted { output: None },
            },
            Body::Interrupt,
            Body::Settings(Overrides::default()),
        ];
        for body in &bodies {
            match body {
                Body::User { .. }
                | Body::Queued { .. }
                | Body::Steer { .. }
                | Body::Progress { .. }
                | Body::Interrupt
                | Body::Settings(_)
                | Body::Assistant { .. }
                | Body::ToolCall { .. }
                | Body::ToolProcess { .. }
                | Body::ToolResult { .. }
                | Body::ModelCall { .. }
                | Body::ModelResult { .. }
                | Body::Hook { .. }
                | Body::Ask { .. }
                | Body::Link { .. }
                | Body::Watch { .. }
                | Body::Fired { .. }
                | Body::Environment(_)
                | Body::Extensions { .. }
                | Body::Ext { .. }
                | Body::Context { .. }
                | Body::Command { .. }
                | Body::CommandResult { .. } => {}
            }
            let json = serde_json::to_value(body).unwrap();
            assert!(known_kind(json["kind"].as_str().unwrap()), "{json}");
        }
        let msgs = [
            DaemonMsg::Welcome {
                proto: PROTO,
                build: "b".into(),
                caps: Caps::ALL,
                client: 1,
                commands: vec![CommandInfo {
                    name: "model".into(),
                    description: "switch model".into(),
                }],
            },
            DaemonMsg::Mismatch {
                supported: PROTO,
                min: None,
            },
            DaemonMsg::Draining,
            DaemonMsg::Attached {
                session: child,
                workspace: "/w".into(),
            },
            DaemonMsg::Entry(Entry::new(1, 0, Body::Ask { call: 1 })),
            DaemonMsg::Delta {
                call: 1,
                text: "t".into(),
            },
            DaemonMsg::Synced { seq: 1 },
            DaemonMsg::Sessions { sessions: vec![] },
            DaemonMsg::Settings {
                settings: HomeSettings::default(),
                error: None,
            },
            DaemonMsg::Status {
                pid: 1,
                build: "b".into(),
                started: 0,
                clients: 1,
                known: 0,
            },
            DaemonMsg::Recap(Recap::default()),
            DaemonMsg::Ask {
                call: 1,
                tool: "t".into(),
                input: Json::null(),
            },
            DaemonMsg::Error {
                message: "m".into(),
            },
            DaemonMsg::Snapshot(Box::default()),
            DaemonMsg::Ack {
                origin: Origin {
                    client: 1,
                    client_seq: 1,
                },
            },
            DaemonMsg::Rejected {
                origin: Origin {
                    client: 1,
                    client_seq: 1,
                },
                reason: Rejection::Stale,
                message: String::new(),
            },
        ];
        for msg in &msgs {
            match msg {
                DaemonMsg::Unknown { .. } => unreachable!(),
                DaemonMsg::Welcome { .. }
                | DaemonMsg::Mismatch { .. }
                | DaemonMsg::Draining
                | DaemonMsg::Attached { .. }
                | DaemonMsg::Entry(_)
                | DaemonMsg::Delta { .. }
                | DaemonMsg::Synced { .. }
                | DaemonMsg::Sessions { .. }
                | DaemonMsg::Settings { .. }
                | DaemonMsg::Status { .. }
                | DaemonMsg::Recap(_)
                | DaemonMsg::Ask { .. }
                | DaemonMsg::Error { .. }
                | DaemonMsg::Snapshot(_)
                | DaemonMsg::Ack { .. }
                | DaemonMsg::Rejected { .. } => {}
            }
            let json = serde_json::to_vec(msg).unwrap();
            let tags: Value = serde_json::from_slice(&json).unwrap();
            assert!(known_msg(tags["t"].as_str().unwrap()), "{tags}");
            assert!(!matches!(
                DaemonMsg::from_json(&json),
                Ok(DaemonMsg::Unknown { .. })
            ));
        }
    }

    #[test]
    fn calls_without_cards_keep_their_shape_and_new_entries_are_flat() {
        let old = r#"{"seq":3,"epoch":0,"kind":"tool_result","call":2,"outcome":{"status":"done","output":{"exit":0,"stderr":"","stdout":"hi\n"}}}"#;
        let e: Entry = serde_json::from_str(old).unwrap();
        assert!(matches!(&e.body, Body::ToolResult { card: None, .. }));
        assert_eq!(serde_json::to_string(&e).unwrap(), old);
        let carded = Body::ToolCall {
            tool: "bash".into(),
            input: serde_json::json!({ "command": "ls" }).into(),
            id: None,
            card: Some(Card {
                title: "$ ls".into(),
                ..Card::default()
            }),
        };
        assert_eq!(
            serde_json::to_string(&carded).unwrap(),
            r#"{"kind":"tool_call","tool":"bash","input":{"command":"ls"},"card":{"title":"$ ls"}}"#
        );
        let ext = Body::Ext {
            ext: "shell".into(),
            state: "cwd".into(),
            data: serde_json::json!("/w").into(),
        };
        assert_eq!(
            serde_json::to_string(&ext).unwrap(),
            r#"{"kind":"ext","ext":"shell","state":"cwd","data":"/w"}"#
        );
    }

    #[test]
    fn interrupted_outcomes_keep_their_shape_and_new_entries_are_flat() {
        let old = r#"{"seq":3,"epoch":0,"kind":"tool_result","call":2,"outcome":{"status":"interrupted"}}"#;
        let e = Entry::from_json(old.as_bytes()).unwrap();
        assert!(matches!(
            &e.body,
            Body::ToolResult {
                outcome: Outcome::Interrupted { output: None },
                ..
            }
        ));
        assert_eq!(serde_json::to_string(&e).unwrap(), old);
        let bodies = [
            Body::Queued {
                text: "q".into(),
                steer: false,
            },
            Body::Steer {
                text: "s".into(),
                queued: 4,
            },
            Body::Progress {
                call: 2,
                text: "p\n".into(),
            },
            Body::ModelResult {
                call: 2,
                outcome: Outcome::Interrupted {
                    output: Some("so far".into()),
                },
            },
            Body::Interrupt,
            Body::Settings(Overrides {
                model: Some("m".into()),
            }),
        ];
        let json: Vec<_> = bodies
            .iter()
            .map(|b| serde_json::to_string(&Entry::new(5, 1, b.clone())).unwrap())
            .collect();
        assert_eq!(
            json,
            [
                r#"{"seq":5,"epoch":1,"kind":"queued","text":"q"}"#,
                r#"{"seq":5,"epoch":1,"kind":"steer","text":"s","queued":4}"#,
                r#"{"seq":5,"epoch":1,"kind":"progress","call":2,"text":"p\n"}"#,
                r#"{"seq":5,"epoch":1,"kind":"model_result","call":2,"outcome":{"status":"interrupted","output":"so far"}}"#,
                r#"{"seq":5,"epoch":1,"kind":"interrupt"}"#,
                r#"{"seq":5,"epoch":1,"kind":"settings","model":"m"}"#,
            ]
        );
        for (body, json) in bodies.iter().zip(&json) {
            assert_eq!(&Entry::from_json(json.as_bytes()).unwrap().body, body);
        }
        let call = Body::ToolCall {
            tool: "t".into(),
            input: Json::null(),
            id: Some("call_1".into()),
            card: None,
        };
        let reserved = Entry {
            uuid: Some(Uuid::from_u128(7)),
            parent: Some(Uuid::from_u128(6)),
            ..Entry::new(1, 1, call)
        };
        let json = serde_json::to_string(&reserved).unwrap();
        assert_eq!(Entry::from_json(json.as_bytes()).unwrap(), reserved);
    }

    #[test]
    fn link_entries_are_flat() {
        let child = SessionId(Uuid::from_u128(1));
        let entries = [
            Body::Link {
                child,
                input: "hi".into(),
            },
            Body::Watch { child },
            Body::Fired {
                watch: 2,
                outcome: Outcome::Interrupted { output: None },
            },
        ];
        let json: Vec<_> = entries
            .iter()
            .map(|b| serde_json::to_string(b).unwrap())
            .collect();
        let id = "00000000-0000-0000-0000-000000000001";
        assert_eq!(
            json,
            [
                format!(r#"{{"kind":"link","child":"{id}","input":"hi"}}"#),
                format!(r#"{{"kind":"watch","child":"{id}"}}"#),
                r#"{"kind":"fired","watch":2,"outcome":{"status":"interrupted"}}"#.to_owned(),
            ]
        );
    }
}
