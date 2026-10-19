//! Wire types shared by the daemon and every client, plus framing.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;

use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The protocol this build speaks best.
pub const PROTO: u32 = 2;

/// The oldest protocol this build still speaks: a peer whose range
/// overlaps `PROTO_MIN..=PROTO` is served at the highest version both speak.
pub const PROTO_MIN: u32 = 2;

const MAX_FRAME: u32 = 16 * 1024 * 1024;

#[macro_use]
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

/// An entry's number in its session's log, from 1: where a fact is, and
/// for an intent, its call's id. `0` names no entry, so `Option<Seq>` is
/// as small as a seq.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct Seq(NonZeroU64);

impl Seq {
    pub const fn new(seq: u64) -> Option<Self> {
        match NonZeroU64::new(seq) {
            Some(seq) => Some(Self(seq)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A call's id: the seq of its intent, which no other kind of number can
/// be mistaken for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct CallId(pub Seq);

impl CallId {
    pub const fn new(seq: u64) -> Option<Self> {
        match Seq::new(seq) {
            Some(seq) => Some(Self(seq)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The generation of the lease a log's writer holds: a store refuses the
/// appends of a writer whose lease was superseded.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
#[repr(transparent)]
pub struct Epoch(pub u32);

/// A client, as it names itself in its hello: the same across its
/// connections and the daemon's restarts, so that what it asked for once
/// is done once (with [`ClientSeq`], an [`Author`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct ClientId(pub Uuid);

impl ClientId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

/// A client's number for one of its mutations, from 1, rising.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct ClientSeq(NonZeroU64);

impl ClientSeq {
    pub const fn new(seq: u64) -> Option<Self> {
        match NonZeroU64::new(seq) {
            Some(seq) => Some(Self(seq)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Who a fact came from: a client's mutation, named durably. Logged with
/// the fact, so a resend after a reconnect or a restart is known for what
/// it is from the log alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Author {
    pub client: ClientId,
    pub seq: ClientSeq,
}

/// What a JSONL line of a session's log holds and, tagged `"t":"entry"`,
/// what the wire carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub seq: u64,
    /// The lease its writer held: a store refuses the appends of a writer
    /// whose lease was superseded.
    pub epoch: u32,
    #[serde(flatten)]
    pub body: Body,
}

impl Entry {
    pub const fn new(seq: u64, epoch: u32, body: Body) -> Self {
        Self { seq, epoch, body }
    }
}

kinds! {
    /// What an entry records. Each fact is logged once, and anything that
    /// can be computed from facts is a fold over them
    /// ([`ledger::Ledger`], [`view::SessionView`]).
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub enum Body: kind = "kind" in EntryFields, class {
        // ── conversation ───────────────────────────────────────────────
        /// Input, logged on arrival so that it survives a crash and every
        /// client sees it. `taken` when it started work as it arrived;
        /// otherwise it waits, in order, for a `take`. A `steer` goes to
        /// the running turn at its next step boundary rather than after it.
        Input "input" { text: String, mode: Mode = default, taken: bool = default, origin: Option<Author> = default } = Durable;
        /// Waiting input `input` was taken up: it starts a turn, or a
        /// `steer` is given to the running one.
        Take "take" { input: Seq } = Durable;
        /// The reply of an agent that is not a model's.
        Assistant "assistant" { text: String } = Durable;
        /// Facts about where the session runs (its directory, the workspace's
        /// agent notes), which the model reads as a user message ahead of the
        /// conversation. Logged once, before the first model call, so the
        /// system prompt stays the same across sessions and cacheable.
        Context "context" { text: String } = Durable;
        // ── calls: each intent is logged before the effect it names ────
        /// The committed intent to ask a model for a reply; a retry is an
        /// intent of its own, its `attempt` one more. With `choice` none the
        /// model is offered the tools but may call none: a turn's last call.
        ModelCall "model_call" { model: String, attempt: u16 = default, choice: ToolChoice = default } = Durable;
        /// The reply; `Done` carries the whole `llm::Response`.
        ModelResult "model_result" { call: u64, outcome: Outcome } = Durable;
        /// The committed intent to run a tool. Its seq is the call's id and
        /// idempotency key; `id` is the model's own id for the call, under which
        /// its result goes back. `by` is who called it: the model, unless the
        /// user ran it as a command or an extension did.
        ToolCall "tool_call" {
            tool: String,
            input: Json,
            id: Option<String> = default,
            /// How clients show the call, as the tool presents it.
            card: Option<Card> = default,
            by: Audience = default,
        } = Durable;
        /// The process group running a call, so recovery can kill it. Only
        /// meaningful within the same `boot`: pgids are reused across reboots.
        /// A `background` group outlives its call: the session owns it until
        /// the session ends.
        ToolProcess "tool_process" { call: u64, pgid: u32, boot: String, background: bool = default } = Durable;
        /// Output tool call `call` produced since its last progress entry,
        /// committed as it runs: what an interrupted call reports, and what
        /// clients show meanwhile.
        Progress "progress" { call: u64, text: String } = Durable;
        ToolResult "tool_result" {
            call: u64,
            outcome: Outcome,
            /// How clients show the outcome, as the tool presents it.
            card: Option<Card> = default,
        } = Durable;
        /// What extension `by` decided at a decision point, about call
        /// `call` when it is about one; logged after the call's intent. Only
        /// decisions that change something are logged.
        Decision "decision" { by: String, call: Option<CallId> = default, #[serde(flatten)] d: Verdict } = Durable, read decode::decision_entry;
        /// Call `call` waits on `on`: until it is answered it is in the
        /// session's inbox, delivered to every client. Logged only for a wait
        /// no decision implies (an `ask` permission implies an approval).
        Wait "wait" { call: CallId, #[serde(flatten)] on: WaitOn } = Durable, read decode::wait_entry;
        /// The answer to call `call`'s wait, from outside the session (a user's
        /// approval is `true` or `false`). The first one wins.
        Answer "answer" { call: CallId, value: Json, origin: Option<Author> = default } = Durable;
        /// The turn was interrupted: what the agent asked for and had not
        /// started is dropped.
        Interrupt "interrupt" { origin: Option<Author> = default } = Durable;
        // ── topology ───────────────────────────────────────────────────
        /// The committed intent to start session `child` with `input`. Its seq
        /// is the call that owns the child; the child's id is chosen before the
        /// commit, so recovery finds the child instead of making another.
        Link "link" { child: SessionId, input: String } = Durable;
        /// A durable watch on `child`: when it settles, its result comes back
        /// as a `fired` under this entry's seq, which is the idempotency key.
        Watch "watch" { child: SessionId } = Durable;
        /// Watch `watch` fired, with its child's result. Logged at most once
        /// per watch.
        Fired "fired" { watch: u64, outcome: Outcome } = Durable;
        // ── environment and extensions ─────────────────────────────────
        /// The environment the session's commands run in from here on, as the
        /// attaching client captured it, secrets redacted.
        Environment "environment" (Environment) = Durable, read decode::environment_entry;
        /// The extensions the session runs with from here on, in install order:
        /// a new session's first entry, logged again whenever they change.
        Extensions "extensions" { installed: Vec<Installed> } = Durable;
        /// A change to extension `ext`'s durable state `state`: folding these
        /// in log order rebuilds it.
        Ext "ext" { ext: String, state: String, data: Json } = Durable;
        /// What the model sees from here on: the entries `target..=upto`
        /// (or just `target`) are left out of its context, or stand as
        /// `replace`. The log itself is never rewritten.
        Edit "edit" { target: Seq, upto: Option<Seq> = default, replace: Option<Json> = default } = Durable;

    }
    else Err(serde_json::Error::custom("unknown entry kind"))
}

impl Body {
    /// An interrupt no client asked for.
    pub const INTERRUPT: Self = Self::Interrupt { origin: None };
}

/// How input waits: for the turn running to end, or for its next step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Queue,
    Steer,
}

/// What a model call may do with the tools it is offered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Call any, or none.
    #[default]
    Auto,
    /// Call none: the tools stay offered, so the request's prefix, and the
    /// provider's cache of it, stay as they were.
    None,
}

/// Who called a tool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    #[default]
    Model,
    /// The user, by a command.
    User,
    /// An extension.
    Plugin,
}

kinds! {
    /// What a call waits on, as a `wait` entry holds it beside the call.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub enum WaitOn: on = "on" in EntryFields {
        /// A user's approval.
        Approval "approval";
        /// A user's input in a dialog the call shows.
        Dialog "dialog" { ui: Json };
        /// A client that runs tool `tool`, of which none is attached.
        Client "client" { tool: String };
        /// The child session linked at `link`.
        Child "child" { link: Seq };
    }
    else Err(serde_json::Error::custom("unknown wait"))
}

kinds! {
    /// What an extension decided, by decision point, as a `decision` entry
    /// holds it beside its call: the call names the tool.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub enum Verdict: d = "d" in EntryFields {
        /// The request of the model call was replaced before it was sent;
        /// `model` is the model it went to instead, if that changed.
        PrepareStep "prepare_step" { model: Option<String> = default };
        /// The tool call's input was replaced by `input`.
        PreTool "pre_tool" { input: Json };
        /// The tool call was blocked before it ran.
        Block "block" { reason: String };
        /// The most restrictive permission the extensions gave the call.
        Permission "permission" { permission: Permission };
        /// An extension answered the call's permission `ask` itself.
        Approval "approval" { approved: bool };
        /// The call's outcome was replaced; `original` is what it replaced.
        PostTool "post_tool" { original: Outcome };
        /// The agent stopped and was given `text` to continue with.
        OnStop "on_stop" { text: String };
        /// The user's input was rewritten: the input entry that follows holds
        /// the new text, `original` what it replaced.
        Input "input" { original: String };
        /// The model call failed with `error` and is sent again, for the
        /// `attempt`th time, with the request as the extension left it.
        Retry "retry" { attempt: u16, error: String };
    }
    else Err(serde_json::Error::custom("unknown decision"))
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
    Prefix(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub workspace: PathBuf,
    pub entries: u64,
    pub clients: u32,
    pub activity: Activity,
    pub residency: Residency,
}

/// What a session is doing. A newer build's states this build does not
/// know read as `other`.
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

/// A feature beyond the base protocol. A client offers the ones it handles
/// in its hello and the welcome names those the daemon takes up; what a
/// capability brings is used only on connections that took it up, so adding
/// one needs no protocol bump and leaves other peers as they were. A
/// capability whose shape changes gets a new name. Everything protocol
/// [`PROTO`] has is its baseline: there are none yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {}

impl Cap {
    /// Every capability this build knows.
    pub const ALL: [Self; 0] = [];

    pub const fn name(self) -> &'static str {
        match self {}
    }

    const fn bit(self) -> u32 {
        match self {}
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

    /// What this build's daemon takes up.
    pub const SERVED: Self = Self::ALL;

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

kinds! {
    #[derive(Clone, Debug, Serialize)]
    pub enum ClientMsg: t = "t" in MsgFields {
        /// `build` is the client's staged build id (`<version>+<hash>`); it
        /// speaks protocols `min..=proto`, or just `proto` without `min`, with
        /// capabilities `caps`. Its first messages (attach, input) may follow
        /// right behind, without waiting for the answer: unless that is a
        /// welcome, the daemon acts on none of them. `client` names it across
        /// connections, so that its numbered mutations are logged with their
        /// [`Author`] and a resend, after a reconnect or a daemon restart, is
        /// answered as the first was; without it, the connection is the
        /// client.
        Hello "hello" {
            proto: u32,
            build: String,
            min: Option<u32> = default,
            caps: Caps = default,
            client: Option<ClientId> = default,
        };
        /// `env` is the client's, for the commands the session runs; without
        /// one the session keeps the environment it had. `workspace` is the
        /// directory the client is in: the daemon takes the git working tree it
        /// is in (with `env`'s git variables) as the workspace.
        Attach "attach" {
            target: Target,
            workspace: PathBuf,
            since: u64,
            env: Option<Environment> = default,
            decl: ClientDecl = default,
        };
        /// Replaces this client's declaration.
        Declare "declare" (ClientDecl), read decode::declare_msg;
        /// Replaces the attached session's environment (`/env refresh`).
        Environment "environment" (Environment), read decode::environment_msg;
        Input "input" { text: String, client_seq: Option<u64> = default };
        /// Input for the running turn, given to it at its next step boundary;
        /// with no turn running it is plain input.
        Steer "steer" { text: String, client_seq: Option<u64> = default };
        List "list";
        Detach "detach";
        /// Asks for [`DaemonMsg::Sessions`] now and whenever a session's
        /// summary changes, for as long as the connection lasts.
        WatchSessions "watch_sessions";
        /// Asks for [`DaemonMsg::Status`].
        Status "status";
        /// Stops the daemon, as an idle one stops; the connection closes as it
        /// exits.
        Shutdown "shutdown";
        /// Asks for the [`HomeSettings`], after making `change` to them if
        /// there is one; answered with [`DaemonMsg::Settings`]. The daemon is
        /// the one writer of `$A_HOME/config.toml`.
        Settings "settings" { change: Option<SettingsChange> = default };
        /// Answers the approval tool call `call` waits for. The first answer
        /// logged wins.
        Approve "approve" { call: u64, approved: bool, client_seq: Option<u64> = default };
        /// Stops the attached session's turn: its running model or tool call
        /// and, bottom-up, the turns of the sessions it started.
        Interrupt "interrupt" { client_seq: Option<u64> = default };
    }
    else Err(serde_json::Error::custom("unknown client message"))
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

kinds! {
    #[derive(Clone, Debug, Serialize)]
    pub enum DaemonMsg: t = "t" in MsgFields, class {
        /// The hello is accepted at protocol `proto`. `caps` are the client's
        /// capabilities the daemon takes up, and `client` this connection's id,
        /// as [`Origin`]s name it. `commands` are the slash commands the daemon
        /// runs, for help and completion.
        Welcome "welcome" {
            proto: u32,
            build: String,
            caps: Caps = default,
            client: u32 = default,
            commands: Vec<CommandInfo> = default,
        } = Control;
        /// The daemon speaks protocols `min..=supported` (just `supported`
        /// without `min`), none of the client's.
        Mismatch "mismatch" { supported: u32, min: Option<u32> = default } = Control;
        /// A newer build was staged: the daemon finishes in-flight work, exits,
        /// and closes every connection. Clients then reconnect, which starts the
        /// new build, and re-attach to replay what they missed. Sent as the reply
        /// to a hello, or to connections already open when the drain began.
        Draining "draining" = Control;
        Attached "attached" { session: SessionId, workspace: PathBuf } = Durable;
        Entry "entry" (Entry) = (entry_class), read decode::entry_msg;
        /// Live text of an unfinished model call, of its `attempt`th try: a
        /// client shows the text of the latest only. Never logged: the
        /// committed `model_result` is the fact.
        Delta "delta" { call: u64, text: String, attempt: u16 = default } = Ephemeral;
        /// Replay is complete; everything after this is live.
        Synced "synced" { seq: u64 } = Durable;
        Sessions "sessions" { sessions: Vec<SessionSummary> } = Control;
        /// The answer to [`ClientMsg::Status`].
        Status "status" {
            pid: u32,
            build: String,
            /// Seconds since the Unix epoch at start.
            started: u64,
            /// Open connections, the asking one included.
            clients: u32,
            /// Sessions the daemon knows, loaded or not.
            known: u32,
        } = Control;
        /// The answer to [`ClientMsg::Settings`]: the settings as they are now,
        /// and why its change was not made, if it was not.
        Settings "settings" { settings: HomeSettings, error: Option<String> = default } = Control;
        Error "error" { message: String } = Control;
        /// The whole log-derived view at `view.seq`, in place of the replay up
        /// to it: for an attach from far behind, or a client that fell too far
        /// behind live. Folding it and what follows equals folding the entries
        /// it stands for.
        Snapshot "snapshot" (Box<view::SessionView>) = Durable, read decode::snapshot_msg;
        /// Numbered mutation `origin` was taken; what it causes follows as
        /// entries. A mutation sent without a number is answered only when
        /// refused, with an [`DaemonMsg::Error`].
        Ack "ack" { origin: Origin } = Control;
        /// Numbered mutation `origin` was refused.
        Rejected "rejected" { origin: Origin, reason: Rejection, message: String = default } = Control;
    }
    /// A message, or an entry, of a kind this build does not know: a newer
    /// daemon's. Never sent; decoding yields it so a client skips what it
    /// cannot show instead of dropping the connection. `seq` is the
    /// entry's.
    unknown Unknown { seq: Option<u64> } = Control;
    else Ok(Self::Unknown { seq: None })
}

/// An entry's frame is as its kind is: every one so far is durable (bash
/// progress included: it is the partial output recovery reports). An
/// ephemeral kind (usage ticks, presence) would go to live clients and never
/// be replayed.
const fn entry_class(msg: &DaemonMsg) -> Class {
    match msg {
        DaemonMsg::Entry(e) => e.body.class(),
        _ => Class::Durable,
    }
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
        // Every entry kind is durable so far; see `entry_class`.
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

/// The frame `buf` starts with, once it holds all of it: its length,
/// prefix included, and its body. For a reader that never blocks: it reads
/// what is there and asks again.
pub fn frame_in(buf: &[u8]) -> io::Result<Option<(usize, &[u8])>> {
    let Some(&[a, b, c, d]) = buf.get(..4) else {
        return Ok(None);
    };
    let len = u32::from_le_bytes([a, b, c, d]);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let end = 4 + len as usize;
    Ok(buf.get(4..end).map(|body| (end, body)))
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
        let e = Entry::new(
            1,
            0,
            Body::Input {
                text: "hi".into(),
                mode: Mode::Queue,
                taken: true,
                origin: None,
            },
        );
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"seq":1,"epoch":0,"kind":"input","text":"hi","taken":true}"#
        );
    }

    #[test]
    fn tool_calls_without_a_model_id_keep_their_shape() {
        let line = r#"{"seq":2,"epoch":0,"kind":"tool_call","tool":"sleep","input":{"seconds":1}}"#;
        let e: Entry = serde_json::from_str(line).unwrap();
        assert!(matches!(&e.body, Body::ToolCall { id: None, .. }));
        assert_eq!(serde_json::to_string(&e).unwrap(), line);
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
        assert!(msg(r#"{"t":"entry","seq":4,"epoch":0,"kind":"input"}"#).is_err());
        assert!(msg(r#"{"t":"synced"}"#).is_err());
        assert!(msg(r#"{"seq":1}"#).is_err());
    }

    /// What `kinds!` declares is all there is: every tag a union's encoder
    /// writes is one its decoder reads, and back.
    #[test]
    fn every_kind_is_registered_by_construction() {
        for tag in Body::TAGS {
            let line = format!(r#"{{"seq":1,"epoch":0,"kind":"{tag}"}}"#);
            match Entry::from_json(line.as_bytes()) {
                Ok(e) => assert_eq!(serde_json::to_value(&e).unwrap()["kind"], *tag),
                Err(e) => assert!(e.to_string().starts_with("missing field"), "{tag}: {e}"),
            }
        }
        for tag in DaemonMsg::TAGS {
            let msg = DaemonMsg::from_json(format!(r#"{{"t":"{tag}"}}"#).as_bytes());
            assert!(!matches!(msg, Ok(DaemonMsg::Unknown { .. })), "{tag}");
        }
        for tag in ClientMsg::TAGS {
            let msg = ClientMsg::from_json(format!(r#"{{"t":"{tag}"}}"#).as_bytes());
            assert!(
                msg.as_ref().map_or_else(
                    |e| e.to_string().starts_with("missing field"),
                    |m| serde_json::to_value(m).unwrap()["t"] == *tag
                ),
                "{tag}: {msg:?}"
            );
        }
        let entry = DaemonMsg::Entry(Entry::new(1, 0, Body::INTERRUPT));
        assert_eq!(serde_json::to_value(&entry).unwrap()["t"], "entry");
        assert!(DaemonMsg::knows("entry") && !DaemonMsg::knows("hello"));
        assert!(Verdict::knows("retry") && !Body::knows("retry"));
    }

    /// The conversation's and the calls' kinds, as a log line holds them.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn entries_are_flat_and_read_back() {
        let author = Author {
            client: ClientId(Uuid::from_u128(0xc)),
            seq: ClientSeq::new(7).unwrap(),
        };
        let seq = |n| Seq::new(n).unwrap();
        let bodies = [
            Body::Input {
                text: "hi".into(),
                mode: Mode::Queue,
                taken: true,
                origin: Some(author),
            },
            Body::Input {
                text: "also".into(),
                mode: Mode::Steer,
                taken: false,
                origin: None,
            },
            Body::Take { input: seq(2) },
            Body::ModelCall {
                model: "m".into(),
                attempt: 1,
                choice: ToolChoice::None,
            },
            Body::Decision {
                by: "policy".into(),
                call: CallId::new(4),
                d: Verdict::Permission {
                    permission: Permission::Ask,
                },
            },
            Body::Decision {
                by: "hook".into(),
                call: CallId::new(3),
                d: Verdict::PrepareStep {
                    model: Some("cheap".into()),
                },
            },
            Body::Decision {
                by: "hook".into(),
                call: CallId::new(4),
                d: Verdict::PostTool {
                    original: Outcome::Failed { error: "e".into() },
                },
            },
            Body::Wait {
                call: CallId::new(4).unwrap(),
                on: WaitOn::Child { link: seq(3) },
            },
            Body::Answer {
                call: CallId::new(4).unwrap(),
                value: true.into(),
                origin: Some(author),
            },
            Body::Interrupt {
                origin: Some(author),
            },
            Body::Edit {
                target: seq(2),
                upto: Some(seq(4)),
                replace: None,
            },
        ];
        let json: Vec<_> = bodies
            .iter()
            .map(|b| serde_json::to_string(&Entry::new(5, 1, b.clone())).unwrap())
            .collect();
        let who = r#""origin":{"client":"00000000-0000-0000-0000-00000000000c","seq":7}"#;
        let head = r#"{"seq":5,"epoch":1,"kind":"#;
        assert_eq!(
            json,
            [
                format!(r#"{head}"input","text":"hi","taken":true,{who}}}"#),
                format!(r#"{head}"input","text":"also","mode":"steer"}}"#),
                format!(r#"{head}"take","input":2}}"#),
                format!(r#"{head}"model_call","model":"m","attempt":1,"choice":"none"}}"#),
                format!(
                    r#"{head}"decision","by":"policy","call":4,"d":"permission","permission":"ask"}}"#
                ),
                format!(
                    r#"{head}"decision","by":"hook","call":3,"d":"prepare_step","model":"cheap"}}"#
                ),
                format!(
                    r#"{head}"decision","by":"hook","call":4,"d":"post_tool","original":{{"status":"failed","error":"e"}}}}"#
                ),
                format!(r#"{head}"wait","call":4,"on":"child","link":3}}"#),
                format!(r#"{head}"answer","call":4,"value":true,{who}}}"#),
                format!(r#"{head}"interrupt",{who}}}"#),
                format!(r#"{head}"edit","target":2,"upto":4}}"#),
            ]
        );
        for (body, json) in bodies.iter().zip(&json) {
            assert_eq!(
                &Entry::from_json(json.as_bytes()).unwrap().body,
                body,
                "{json}"
            );
        }
        for bad in [
            r#"{"seq":1,"epoch":0,"kind":"wait","call":0,"on":"approval"}"#,
            r#"{"seq":1,"epoch":0,"kind":"take","input":0}"#,
            r#"{"seq":1,"epoch":0,"kind":"decision","by":"h","d":"later"}"#,
        ] {
            assert!(Entry::from_json(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn calls_without_cards_keep_their_shape_and_ext_entries_are_flat() {
        let line = r#"{"seq":3,"epoch":0,"kind":"tool_result","call":2,"outcome":{"status":"done","output":{"exit":0,"stderr":"","stdout":"hi\n"}}}"#;
        let e: Entry = serde_json::from_str(line).unwrap();
        assert!(matches!(&e.body, Body::ToolResult { card: None, .. }));
        assert_eq!(serde_json::to_string(&e).unwrap(), line);
        let carded = Body::ToolCall {
            tool: "bash".into(),
            input: serde_json::json!({ "command": "ls" }).into(),
            id: None,
            by: Audience::Model,
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
    fn interrupted_outcomes_keep_their_shape_and_progress_is_flat() {
        let line = r#"{"seq":3,"epoch":0,"kind":"tool_result","call":2,"outcome":{"status":"interrupted"}}"#;
        let e = Entry::from_json(line.as_bytes()).unwrap();
        assert!(matches!(
            &e.body,
            Body::ToolResult {
                outcome: Outcome::Interrupted { output: None },
                ..
            }
        ));
        assert_eq!(serde_json::to_string(&e).unwrap(), line);
        let bodies = [
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
            Body::INTERRUPT,
        ];
        let json: Vec<_> = bodies
            .iter()
            .map(|b| serde_json::to_string(&Entry::new(5, 1, b.clone())).unwrap())
            .collect();
        assert_eq!(
            json,
            [
                r#"{"seq":5,"epoch":1,"kind":"progress","call":2,"text":"p\n"}"#,
                r#"{"seq":5,"epoch":1,"kind":"model_result","call":2,"outcome":{"status":"interrupted","output":"so far"}}"#,
                r#"{"seq":5,"epoch":1,"kind":"interrupt"}"#,
            ]
        );
        for (body, json) in bodies.iter().zip(&json) {
            assert_eq!(&Entry::from_json(json.as_bytes()).unwrap().body, body);
        }
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
