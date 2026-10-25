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
pub const PROTO: u32 = 31;

/// The oldest protocol this build still speaks: a peer whose range
/// overlaps `PROTO_MIN..=PROTO` is served at the highest version both speak.
pub const PROTO_MIN: u32 = 31;

/// Ordinary payloads leave 4 KiB per reserved control slot for cancellation.
#[derive(Clone, Copy)]
pub struct QueueBudget {
    pub bytes: usize,
    pub requests: usize,
    pub control_reserve: usize,
}

impl Default for QueueBudget {
    fn default() -> Self {
        Self {
            bytes: 32 << 20,
            requests: 1024,
            control_reserve: 16,
        }
    }
}

impl QueueBudget {
    pub fn admits(&self, bytes: usize, requests: usize, control: bool) -> bool {
        let reserve = if control { 0 } else { self.control_reserve };
        requests < self.requests.saturating_sub(reserve)
            && bytes <= self.bytes.saturating_sub(reserve.saturating_mul(4096))
    }
}

// One writer type keeps counting and buffered serde instantiations shared.
struct EncodeTarget<'a> {
    bytes: Option<&'a mut Vec<u8>>,
    count: usize,
}

impl Write for EncodeTarget<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_all(bytes)?;
        Ok(bytes.len())
    }
    #[inline(never)]
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(out) = &mut self.bytes {
            out.extend_from_slice(bytes);
        }
        self.count += bytes.len();
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// What a frame's body is, in the byte after its length: a reader refuses
/// a kind it does not know, so binary frames (a blob's chunks) can follow
/// without going through base64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Json = 0,
}

impl FrameKind {
    fn of(byte: u8) -> io::Result<Self> {
        match byte {
            0 => Ok(Self::Json),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown frame kind {byte}"),
            )),
        }
    }
}

/// The length, then the kind.
const HEADER: usize = 5;

pub mod blob;
#[macro_use]
mod decode;
pub mod ledger;
mod peer;
pub mod publication;
pub mod view;

pub use blob::{BlobId, BlobOrigin, BlobRef, Held, Payload};
pub use json::Json;
pub use peer::{Host, MachineId, Peer, Role, Via};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// The id no session has: what a kernel that was not told its own
    /// addresses itself by.
    pub const NIL: Self = Self(Uuid::nil());

    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    #[must_use]
    pub fn owned(self, key: &str) -> Self {
        Self(Uuid::new_v5(&self.0, key.as_bytes()))
    }

    /// Short form for display: the UUID tail.
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

impl From<CallId> for Seq {
    fn from(call: CallId) -> Self {
        call.0
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
    pub seq: Seq,
    /// The lease its writer held: a store refuses the appends of a writer
    /// whose lease was superseded.
    pub epoch: Epoch,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<Json>,
    #[serde(flatten)]
    pub body: Body,
}

impl Entry {
    pub const fn new(seq: Seq, epoch: Epoch, body: Body) -> Self {
        Self {
            seq,
            epoch,
            execution: None,
            body,
        }
    }
}

/// A name is stable within one parent call, including across recovery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invocation {
    pub parent: CallId,
    pub name: String,
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
        /// the running turn at its next step boundary rather than after it;
        /// a `force`d one cuts the model call short and detaches the
        /// running calls that can go on in the background, so that
        /// boundary comes now.
        Input "input" { segments: Segments, mode: Mode = default, taken: bool = default, origin: Option<Author> = default, force: bool = default } = Durable;
        /// Queued item `input` was taken up: it starts a turn, is read by
        /// the next model call, or, `into` a call, is read with that
        /// call's result, keeping each call beside its result.
        Take "take" { input: Seq, into: Option<CallId> = default } = Durable;
        /// Queued item `input` was taken back before it was taken up: it is
        /// never read. Whichever of `withdraw` and `take` is logged first
        /// wins. An edit is a withdraw and the edited input after it.
        Withdraw "withdraw" { input: Seq, origin: Option<Author> = default } = Durable;
        /// The reply of an agent that is not a model's.
        Assistant "assistant" { text: String } = Durable;
        /// Facts about where the session runs (its directory, the workspace's
        /// agent notes), which the model reads as a user message ahead of the
        /// conversation. Logged once, before the first model call, so the
        /// system prompt stays the same across sessions and cacheable.
        Context "context" { text: String, publication: Option<publication::Publication> = default } = Durable;
        // ── calls: each intent is logged before the effect it names ────
        /// The committed intent to ask a model for a reply. A retry decision
        /// commits before this call can be sent again. With `choice` none the
        /// model is offered the tools but may call none: a turn's last call.
        /// A side request an extension makes on behalf of call `for` (to
        /// compact, judge, title) names it and its `purpose`: its reply is
        /// the extension's, never the conversation's.
        ModelCall "model_call" {
            model: String,
            attempt: u16 = default,
            choice: ToolChoice = default,
            r#for: Option<CallId> = default,
            purpose: Option<String> = default,
            request: Option<Json> = default,
        } = Durable;
        /// The reply; `Done` carries the whole `llm::Response`. `for` names
        /// a side request's owner even when an edit removes its intent.
        ModelResult "model_result" { call: CallId, r#for: Option<CallId> = default, outcome: Outcome, failure: Option<Json> = default } = Durable;
        /// The committed intent to run a tool. Its seq is the call's id and
        /// idempotency key; `id` is the model's own id for the call, under which
        /// its result goes back. `by` is who called it: the model, unless the
        /// user ran it as a command or an extension did.
        ToolCall "tool_call" {
            invoke: Option<Invocation> = default,
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
        ToolProcess "tool_process" { call: CallId, pgid: u32, boot: String, background: bool = default } = Durable;
        /// Extension-owned facts carry no execution authority.
        Fact "fact" { fact: ExtensionFact } = Durable;
        /// Output tool call `call` produced since its last progress entry,
        /// committed as it runs: what an interrupted call reports, and what
        /// clients show meanwhile.
        Progress "progress" { call: CallId, text: String } = Durable;
        /// Running call `call` was detached: the result logged next is a
        /// placeholder naming its output stream ([`OutputStream`]), and its
        /// outcome comes later, as the session's own envelope
        /// ([`EnvelopeId::DETACHED`]).
        Detached "detached" { call: CallId } = Durable;
        ToolResult "tool_result" {
            call: CallId,
            outcome: Outcome,
            /// How clients show the outcome, as the tool presents it.
            card: Option<Card> = default,
            /// What the outcome references outside the log, which the
            /// store keeps for as long as the entry is.
            held: Vec<Held> = default,
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
        Link "link" { child: SessionId, input: String, intent: Json } = Durable;
        /// The committed intent to send `payload` to `recipients`, the set
        /// the session's route resolved its address to, once: each is
        /// delivered an [`Envelope`] under the one id.
        Outbox "outbox" { envelope_id: EnvelopeId, recipients: Vec<SessionId>, payload: String } = Durable;
        /// An envelope taken into this session's queue, once per id.
        Enqueue "enqueue" { envelope: Envelope } = Durable;
        /// Recipient `recipient` took in envelope `envelope_id`, or refused it.
        Ack "ack" { envelope_id: EnvelopeId, recipient: SessionId, refusal: Option<InboxRefusal> = default } = Durable;
        Spawned "spawned" { intent: Json } = Durable;
        /// A durable watch on `child`: when it settles, its result comes back
        /// as a `fired` under this entry's seq, which is the idempotency key.
        Watch "watch" { child: SessionId } = Durable;
        /// Watch `watch` fired, with its child's result. Logged at most once
        /// per watch. A call waiting on the child goes on with it; otherwise
        /// it reaches the agent as the session's own envelope
        /// ([`EnvelopeId::CHILD`]).
        Fired "fired" { watch: Seq, outcome: Outcome } = Durable;
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
    /// An entry of a kind this build does not know, such as a newer
    /// build's: folds pass over it, and it is written back as the text it
    /// was read as, `raw`, the whole entry.
    unknown Unknown { kind: String, raw: Json } = Durable;
    else Err(serde_json::Error::custom("unknown entry kind"))
}

impl Body {
    /// An interrupt no client asked for.
    pub const INTERRUPT: Self = Self::Interrupt { origin: None };
}

/// A piece of input as its client composed it: what the user typed, what
/// they pasted, and what they attached. A client shows a paste as a
/// placeholder and the model reads it tagged as one, so the two are never
/// confused with typed text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    Text(String),
    Paste(String),
    /// A paste the daemon keeps as a blob: one too large to inline, as the
    /// `input` point decides.
    PasteRef(BlobRef),
    Attachment(BlobRef),
}

/// Input, a segment at a time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Segments(pub Vec<Segment>);

impl Segments {
    /// What the user typed and pasted, as one text; what is kept by
    /// reference adds nothing.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for segment in &self.0 {
            if let Segment::Text(text) | Segment::Paste(text) = segment {
                out.push_str(text);
            }
        }
        out
    }

    /// The text, when the input is typed text alone: a command, or a
    /// message a program wrote.
    pub fn as_text(&self) -> Option<&str> {
        match &self.0[..] {
            [] => Some(""),
            [Segment::Text(text)] => Some(text),
            _ => None,
        }
    }

    /// The bytes its texts hold.
    pub fn capacity(&self) -> usize {
        (self.0.iter())
            .map(|segment| match segment {
                Segment::Text(text) | Segment::Paste(text) => text.capacity(),
                Segment::PasteRef(_) | Segment::Attachment(_) => size_of::<BlobRef>(),
            })
            .sum()
    }
}

impl From<String> for Segments {
    fn from(text: String) -> Self {
        Self(vec![Segment::Text(text)])
    }
}

impl From<&str> for Segments {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl From<Vec<Segment>> for Segments {
    fn from(segments: Vec<Segment>) -> Self {
        Self(segments)
    }
}

impl PartialEq<str> for Segments {
    fn eq(&self, text: &str) -> bool {
        self.as_text() == Some(text)
    }
}

impl PartialEq<&str> for Segments {
    fn eq(&self, text: &&str) -> bool {
        self == *text
    }
}

impl fmt::Display for Segments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
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

/// Who called a tool. A caller a newer build has counts as neither the
/// model nor the user.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    #[default]
    Model,
    /// The user, by a command.
    User,
    /// An extension.
    #[serde(other)]
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
        /// The model call was answered by the extension, not sent: its
        /// result is the extension's answer.
        Answered "answered";
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
        /// the new input, `original` what it replaced.
        Input "input" { original: Segments };
        /// The model call failed with `error` and is sent again, for the
        /// `attempt`th time, with the request as the extension left it.
        Retry "retry" { attempt: u16, error: String };
        SideRetry "side_retry" { attempt: u16, failure: Json, after_ms: u64 };
    }
    else Err(serde_json::Error::custom("unknown decision"))
}

/// Extension-owned facts are externally tagged to avoid nested tag buffering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionFact {
    Annotation(Annotation),
}

/// Execution identity stays qualified when history is copied into another log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AttemptId {
    pub executing_session: SessionId,
    pub call: CallId,
    pub attempt: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Charge {
    pub currency: String,
    pub nanos: u64,
}

/// Execution observations belong to an existing settlement, never an admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallFact {
    pub id: AttemptId,
    pub model: String,
    pub role: String,
    pub runner: String,
    pub agent: String,
    pub side: bool,
    /// Wall time at settlement; never used to reconstruct elapsed time.
    pub unix_ms: u64,
    pub clock_epoch: SessionId,
    pub start_ns: u64,
    pub elapsed_ns: Option<u64>,
    pub usage: Option<Json>,
    pub charge: Option<Charge>,
}

impl CallFact {
    #[inline(never)]
    pub fn to_json(&self) -> serde_json::Result<Json> {
        Json::encode(&(
            (self.id.executing_session, self.id.call, self.id.attempt),
            &self.model,
            &self.role,
            &self.runner,
            &self.agent,
            self.side,
            self.unix_ms,
            self.clock_epoch,
            self.start_ns,
            self.elapsed_ns,
            &self.usage,
            self.charge.as_ref().map(|c| (&c.currency, c.nanos)),
        ))
    }

    #[inline(never)]
    pub fn from_json(json: &Json) -> serde_json::Result<Self> {
        let (
            (executing_session, call, attempt),
            model,
            role,
            runner,
            agent,
            side,
            unix_ms,
            clock_epoch,
            start_ns,
            elapsed_ns,
            usage,
            charge,
        ) = json.parse::<(
            (SessionId, CallId, u16),
            String,
            String,
            String,
            String,
            bool,
            u64,
            SessionId,
            u64,
            Option<u64>,
            Option<Json>,
            Option<(String, u64)>,
        )>()?;
        Ok(Self {
            id: AttemptId {
                executing_session,
                call,
                attempt,
            },
            model,
            role,
            runner,
            agent,
            side,
            unix_ms,
            clock_epoch,
            start_ns,
            elapsed_ns,
            usage,
            charge: charge.map(|(currency, nanos)| Charge { currency, nanos }),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationLifetime {
    /// An answer or cancellation revokes presentation for this question.
    Question,
    /// Presentation lasts only while the target call is open.
    Call,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    pub target: CallId,
    pub lifetime: AnnotationLifetime,
    pub card: Card,
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
    /// How the outcome went, as only the tool knows (`exit 1` is bad, a
    /// diff applied is good): what colours the note.
    #[serde(default, skip_serializing_if = "Mark::is_ok")]
    pub mark: Mark,
}

/// How a call went, as its tool marks its card.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    #[default]
    Ok,
    Bad,
    Warn,
}

impl Mark {
    // A reference, as `skip_serializing_if` passes it.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// An extension a session ran with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub name: String,
    pub version: u32,
    /// Portable code and execution limits for a dynamic extension generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Json>,
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
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self.cwd.capacity()
            + self
                .vars
                .iter()
                .map(|(key, value)| {
                    2 * size_of::<(String, String)>() + key.capacity() + value.capacity()
                })
                .sum::<usize>()
            + self.redacted.capacity() * size_of::<String>()
            + self.redacted.iter().map(String::capacity).sum::<usize>()
    }

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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    #[default]
    Allow,
    /// Runs once someone approves it.
    Ask(ApprovalReason),
    /// Decided outside this run, later.
    Defer,
    Deny,
}

/// The policy fact that requires a person to approve a call. Model-written
/// descriptions never replace this fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReason {
    Mode { mode: String },
    OperatorRule { tool: String, input: Option<String> },
    OutsideWorkspace { path: PathBuf, workspace: PathBuf },
    Classifier { reason: String },
}

impl Permission {
    /// Restrictiveness ignores explanatory data; equal rules retain the
    /// first handler's reason.
    pub const fn rank(&self) -> u8 {
        match self {
            Self::Allow => 0,
            Self::Ask(_) => 1,
            Self::Defer => 2,
            Self::Deny => 3,
        }
    }
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

impl Outcome {
    /// The payload of an envelope that carries it ([`EnvelopeId::CHILD`],
    /// [`EnvelopeId::DETACHED`]).
    pub fn payload(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The outcome such an envelope's payload carries.
    pub fn from_payload(payload: &str) -> Option<Self> {
        serde_json::from_str(payload).ok()
    }
}

/// The durable ancestry of an independently imported conversation prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkParent {
    pub session: SessionId,
    pub seq: Seq,
}

/// Why a conversation prefix could not become an independent session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkError {
    SourceNotFound,
    AmbiguousSource,
    InvalidSequence,
    Unfinished,
    OwnedResources,
    Disabled,
    UnavailableGeneration,
    Viewer,
    ShuttingDown,
    Storage { message: String },
}

impl fmt::Display for ForkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SourceNotFound => "no matching source session",
            Self::AmbiguousSource => "source matches several sessions",
            Self::InvalidSequence => "fork sequence is outside the source history",
            Self::Unfinished => "fork requires a rest point: unfinished calls, approvals and queued actions cannot be copied",
            Self::OwnedResources => "fork requires a prefix without owned background processes or child watches",
            Self::Disabled => "fork extension is disabled",
            Self::UnavailableGeneration => "recorded extension generation unavailable",
            Self::Viewer => "this client is a viewer",
            Self::ShuttingDown => "the daemon is shutting down",
            Self::Storage { message } => message,
        })
    }
}

impl std::error::Error for ForkError {}

impl From<io::Error> for ForkError {
    fn from(error: io::Error) -> Self {
        Self::Storage {
            message: error.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    New,
    /// Copy conversation history into an independent session; never rewinds files.
    Fork {
        session: String,
        seq: Option<Seq>,
    },
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
    /// Why its log is kept as it is, if it is: shown, never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<ReadOnly>,
}

/// Why a session's log opened read-only: this build shows what it reads
/// of it and never writes to it, so nothing in it is lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnly {
    /// The recorded dynamic generation cannot be reconstructed by this registry.
    Extensions,
    /// Its lines from entry `seq` on are damaged or unreadable, and are
    /// not the tail a crash leaves: the entries before it are shown.
    Damaged { seq: u64 },
    /// It was written in log format `version`, which this build does not
    /// write: a newer build's, or one too old to read.
    Format { version: u32 },
}

/// The line `a ls` and a picker show: the short id, what it is doing,
/// where it is, how long it is, where it works, and why it is read-only
/// if it is.
impl fmt::Display for SessionSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}  {:<9}  {:<10}  {:>4} entries  {}",
            self.id.short(),
            self.activity.name(),
            self.residency.name(),
            self.entries,
            self.workspace.display()
        )?;
        if let Some(why) = self.read_only {
            write!(f, "  ({why})")?;
        }
        Ok(())
    }
}

impl fmt::Display for ReadOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Extensions => f.write_str("read-only: recorded extension generation unavailable"),
            Self::Damaged { seq } => write!(f, "read-only: damaged at entry {seq}"),
            Self::Format { version } => write!(f, "read-only: log format {version}"),
        }
    }
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

impl SettingsChange {
    /// The change that takes the settings `from` to `to`: only what
    /// differs, so a save sends nothing another client's save would be
    /// undone by. A blank model or agent is none.
    pub fn between(from: &HomeSettings, to: &HomeSettings) -> Self {
        let given = |s: &Option<String>| s.as_deref().map_or("", str::trim).to_owned();
        let changed = |old: &Option<String>, new: &Option<String>| {
            let (old, new) = (given(old), given(new));
            (old != new).then_some(new)
        };
        let mut env: BTreeMap<String, Option<String>> = from
            .env
            .keys()
            .filter(|name| !to.env.contains_key(*name))
            .map(|name| (name.clone(), None))
            .collect();
        env.extend(
            to.env
                .iter()
                .filter(|(name, value)| from.env.get(*name) != Some(value))
                .map(|(name, value)| (name.clone(), Some(value.clone()))),
        );
        Self {
            model: changed(&from.model, &to.model),
            agent: changed(&from.agent, &to.agent),
            env,
        }
    }
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
    /// Tools this client runs itself, by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// UI sites it renders, such as `dialog` or `status`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<String>,
}

/// A slash command the daemon runs: input `/name args`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
}

/// A query the daemon's extensions answer, for clients to discover rather
/// than know: asked with [`ClientMsg::Query`] about its `scope` or one
/// narrower.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryInfo {
    pub name: String,
    pub description: String,
    pub scope: ScopeKind,
}

/// What a query is asked about. A workspace is named by a directory in it,
/// found as an attach finds its workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Daemon,
    Workspace(PathBuf),
    Session(SessionId),
}

/// The kinds of [`Scope`], widest first: a query of one kind may be asked
/// about any kind after it, which implies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Daemon,
    Workspace,
    Session,
}

impl fmt::Display for ScopeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Daemon => "daemon",
            Self::Workspace => "workspace",
            Self::Session => "session",
        })
    }
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
    /// What it answers waits no longer: the call is not waiting for
    /// approval, the input to withdraw was taken up.
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
        /// client. `machine` is the client's, where that is not the
        /// daemon's: a client of this machine's daemon leaves it out.
        Hello "hello" {
            proto: u32,
            build: String,
            min: Option<u32> = default,
            caps: Caps = default,
            client: Option<ClientId> = default,
            machine: Option<MachineId> = default,
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
        /// Input; a `steer` goes to the running turn at its next step
        /// boundary, and with no turn running is plain input. A `force`d
        /// steer brings that boundary on now: the model call is cut short
        /// and the calls running that can go on in the background do.
        Input "input" { segments: Segments, mode: Mode = default, force: bool = default, client_seq: Option<u64> = default };
        /// Takes back the user's queued input `input` before it is taken up;
        /// refused once it was. Editing it is this, then the new input.
        Withdraw "withdraw" { input: Seq, client_seq: Option<u64> = default };
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
        Approve "approve" { call: CallId, approved: bool, client_seq: Option<u64> = default };
        /// Stops the attached session's turn: its running model or tool call
        /// and, bottom-up, the turns of the sessions it started.
        Interrupt "interrupt" { client_seq: Option<u64> = default };
        /// Asks query `service` about `scope`, never logged: answered by
        /// [`DaemonMsg::Update`]s, if it streams, then one
        /// [`DaemonMsg::Reply`], all naming `id`, which the client picks
        /// and does not reuse while the query runs.
        Query "query" { id: u64, service: String, scope: Scope, body: Json };
        /// Stops query `id`: its reply follows, interrupted unless it was
        /// sent already.
        Cancel "cancel" { id: u64 };
    }
    else Err(serde_json::Error::custom("unknown client message"))
}

impl ClientMsg {
    pub fn retained_bytes(&self) -> usize {
        let mut count = EncodeTarget {
            bytes: None,
            count: size_of::<Self>(),
        };
        let _ = serde_json::to_writer(&mut count, self);
        // Reserve the widest sequence before admission, so numbering cannot
        // increase a mutation's charge in the transport queue.
        if matches!(
            self,
            Self::Input { .. }
                | Self::Approve { .. }
                | Self::Interrupt { .. }
                | Self::Withdraw { .. }
        ) {
            count.count += self
                .client_seq()
                .map_or(br#","client_seq":"#.len() + 20, |seq| {
                    19 - seq.checked_ilog10().unwrap_or(0) as usize
                });
        }
        count.count.max(
            size_of::<Self>()
                + match self {
                    Self::Input { segments, .. } => segments.capacity(),
                    _ => 0,
                },
        )
    }

    pub fn write_frame(&self, writer: &mut dyn Write) -> io::Result<()> {
        write_frame(writer, self)
    }
}

impl DaemonMsg {
    pub fn retained_bytes(&self) -> usize {
        let mut count = EncodeTarget {
            bytes: None,
            count: size_of::<Self>(),
        };
        let _ = serde_json::to_writer(&mut count, self);
        count.count
    }
}

impl ClientMsg {
    pub const INTERRUPT: Self = Self::Interrupt { client_seq: None };

    pub fn input(segments: impl Into<Segments>) -> Self {
        Self::Input {
            segments: segments.into(),
            mode: Mode::Queue,
            force: false,
            client_seq: None,
        }
    }

    pub const fn approve(call: CallId, approved: bool) -> Self {
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
            | Self::Approve { client_seq, .. }
            | Self::Withdraw { client_seq, .. }
            | Self::Interrupt { client_seq } => *client_seq,
            _ => None,
        }
    }

    /// The idempotency key of a mutation (a message that changes the
    /// session), which the client numbers; `None` for any other message.
    pub const fn client_seq_mut(&mut self) -> Option<&mut Option<u64>> {
        match self {
            Self::Input { client_seq, .. }
            | Self::Approve { client_seq, .. }
            | Self::Withdraw { client_seq, .. }
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
            | Self::Settings { .. }
            | Self::Query { .. }
            | Self::Cancel { .. } => None,
        }
    }
}

kinds! {
    #[derive(Clone, Debug, Serialize)]
    pub enum DaemonMsg: t = "t" in MsgFields, class {
        /// The hello is accepted at protocol `proto`. `caps` are the client's
        /// capabilities the daemon takes up. `machine` is the daemon's.
        /// `commands` are the slash commands the daemon runs, for help and
        /// completion; `queries` what it answers [`ClientMsg::Query`] about.
        Welcome "welcome" {
            proto: u32,
            build: String,
            caps: Caps = default,
            machine: MachineId,
            commands: Vec<CommandInfo> = default,
            queries: Vec<QueryInfo> = default,
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
        Delta "delta" { call: CallId, text: String, attempt: u16 = default } = Ephemeral;
        /// Replay is complete; everything after this is live.
        Synced "synced" { seq: u64 } = Durable;
        /// The kernel's activity after a complete burst, ordered after `seq`.
        /// Entries after it invalidate it until the next activity frame.
        Activity "activity" { seq: u64, activity: Activity } = Durable;
        /// A refused fork, rendered once by the shared client rule.
        ForkRejected "fork_rejected" { reason: ForkError } = Control;
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
        Snapshot "snapshot" (Box<view::ViewSnapshot>) = Durable, read decode::snapshot_msg;
        /// This connection's mutation numbered `client_seq` was taken; what
        /// it causes follows as entries. A mutation sent without a number
        /// is answered only when refused, with an [`DaemonMsg::Error`].
        Ack "ack" { client_seq: u64 } = Control;
        /// This connection's mutation numbered `client_seq` was refused.
        Rejected "rejected" { client_seq: u64, reason: Rejection, message: String = default } = Control;
        /// One of query `id`'s streamed partial answers, in order.
        Update "update" { id: u64, body: Json } = Control;
        /// How query `id` ended: its answer, why not, or that it was
        /// cancelled. Nothing about `id` follows.
        Reply "reply" { id: u64, outcome: Outcome } = Control;
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
    /// About the connection rather than the session; ordered unless
    /// [`Frame::replaceable`] permits replacement.
    Control,
}

/// Writes one frame in one write: a little-endian u32 length of what
/// follows, the [`FrameKind`] byte, then the JSON body.
pub fn write_frame<T: Serialize>(w: &mut (impl Write + ?Sized), msg: &T) -> io::Result<()> {
    let mut bytes = Frame::start();
    serde_json::to_writer(
        &mut EncodeTarget {
            bytes: Some(&mut bytes),
            count: 0,
        },
        msg,
    )?;
    prefix(&mut bytes)?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Fills in the length of the frame `bytes` holds after its first four,
/// the kind byte included.
fn prefix(bytes: &mut [u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len() - 4)
        .ok()
        .filter(|&n| n <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    bytes[..4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Connection snapshots whose newest value supersedes an unsent value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replaceable {
    Sessions,
    Status,
    Settings,
}

/// A frame encoded once, length prefix included, for any number of peers:
/// cloning it shares the bytes.
#[derive(Clone, Debug)]
pub struct Frame {
    bytes: Arc<[u8]>,
    /// The entry's seq, for an entry.
    pub seq: Option<u64>,
    pub class: Class,
    pub replaceable: Option<Replaceable>,
}

impl Frame {
    pub fn new(msg: &DaemonMsg) -> io::Result<Self> {
        let mut bytes = Self::start();
        serde_json::to_writer(
            &mut EncodeTarget {
                bytes: Some(&mut bytes),
                count: 0,
            },
            msg,
        )?;
        let mut frame = Self::finish(bytes, None, msg.class())?;
        frame.replaceable = match msg {
            DaemonMsg::Sessions { .. } => Some(Replaceable::Sessions),
            DaemonMsg::Status { .. } => Some(Replaceable::Status),
            DaemonMsg::Settings { .. } => Some(Replaceable::Settings),
            _ => None,
        };
        Ok(frame)
    }

    /// The frame of `DaemonMsg::Entry(entry.clone())`, without the clone:
    /// the entry's own JSON with the tag spliced in first, as serde puts it.
    pub fn entry(entry: &Entry) -> io::Result<Self> {
        let mut bytes = Self::start();
        bytes.extend_from_slice(br#"{"t":"entry""#);
        let at = bytes.len();
        entry.encode(&mut bytes)?;
        bytes[at] = b',';
        Self::finish(bytes, Some(entry.seq.get()), entry.body.class())
    }

    /// Room for the header of a JSON frame, and as much again as
    /// `serde_json::to_vec` starts with.
    fn start() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(132);
        bytes.extend_from_slice(&[0, 0, 0, 0, FrameKind::Json as u8]);
        bytes
    }

    fn finish(mut bytes: Vec<u8>, seq: Option<u64>, class: Class) -> io::Result<Self> {
        prefix(&mut bytes)?;
        Ok(Self {
            bytes: Arc::from(bytes),
            seq,
            class,
            replaceable: None,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The JSON the frame carries, past its header.
    pub fn body(&self) -> &[u8] {
        &self.bytes[HEADER..]
    }

    /// Entry `seq`'s frame from the line a log stores for it, newline
    /// excluded: the bytes encoded at commit, tagged, not encoded again.
    pub fn from_line(seq: u64, line: &[u8]) -> io::Result<Self> {
        const TAG: &[u8] = br#"{"t":"entry","#;
        let fields = line
            .strip_prefix(b"{")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an entry's line"))?;
        let mut bytes = Vec::with_capacity(HEADER + TAG.len() + fields.len());
        bytes.extend_from_slice(&[0, 0, 0, 0, FrameKind::Json as u8]);
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
        let out = &mut EncodeTarget {
            bytes: Some(out),
            count: 0,
        };
        match &self.body {
            Body::Unknown { raw, .. } => out
                .write_all(raw.raw().as_bytes())
                .map_err(serde_json::Error::io),
            _ => serde_json::to_writer(out, self),
        }
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
/// header included, and its JSON body. For a reader that never blocks: it
/// reads what is there and asks again.
pub fn frame_in(buf: &[u8]) -> io::Result<Option<(usize, &[u8])>> {
    let Some(&[a, b, c, d]) = buf.get(..4) else {
        return Ok(None);
    };
    let len = length(u32::from_le_bytes([a, b, c, d]))?;
    let end = 4 + len;
    let Some(frame) = buf.get(4..end) else {
        return Ok(None);
    };
    FrameKind::of(frame[0])?;
    Ok(Some((end, &frame[1..])))
}

/// What a length prefix says follows it: a kind byte at least, and no
/// more than [`MAX_FRAME`].
fn length(len: u32) -> io::Result<usize> {
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    if len == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame without a kind",
        ));
    }
    Ok(len as usize)
}

/// The body of the next frame, read once for every type and stream.
fn read_body(r: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = length(u32::from_le_bytes(len))?;
    let mut kind = [0];
    r.read_exact(&mut kind)?;
    FrameKind::of(kind[0])?;
    let len = len - 1;
    // Grown as the bytes arrive, so a length alone allocates no more than
    // the peer goes on to send.
    let mut body = Vec::with_capacity(len.min(64 * 1024));
    r.take(len as u64).read_to_end(&mut body)?;
    if body.len() < len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Some(body))
}

/// Sender-local identity remains stable across retries and receiver restarts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EnvelopeId {
    pub sender: SessionId,
    pub call: CallId,
    pub purpose: String,
}
impl EnvelopeId {
    /// The purpose of the envelope a child no call waits on settles into,
    /// under its watch: its payload is the child's [`Outcome`].
    pub const CHILD: &str = "child";
    /// The purpose of a detached call's outcome, under the call: its
    /// payload is the [`Outcome`].
    pub const DETACHED: &str = "detached";
    /// The purpose of the end of a process group a call left to the
    /// session, under the call: its payload is text for the model.
    pub const JOB: &str = "job";
}

/// What one recipient takes into its queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: EnvelopeId,
    pub destination: SessionId,
    pub payload: String,
    /// What the payload references outside the log: a detached call's
    /// outcome holds what its result would have.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<Held>,
}

impl Envelope {
    /// An envelope session `session` sends itself, about its own call
    /// `call`: how what it started and stopped waiting on (a child, a
    /// detached call, a background job) reaches it, on the one queue,
    /// deduplicated by the id as any envelope is.
    pub fn own(session: SessionId, call: CallId, purpose: &str, payload: String) -> Self {
        Self {
            id: EnvelopeId {
                sender: session,
                call,
                purpose: purpose.to_owned(),
            },
            destination: session,
            payload,
            held: Vec::new(),
        }
    }

    /// The envelope, holding `held`.
    #[must_use]
    pub fn holding(mut self, held: Vec<Held>) -> Self {
        self.held = held;
        self
    }

    /// Where it comes from, as the queue tells its items apart.
    pub fn source(&self) -> Source {
        if self.id.sender != self.destination {
            Source::Agent(self.id.sender)
        } else if self.id.purpose == EnvelopeId::CHILD {
            Source::Child(self.id.call.0)
        } else {
            Source::Job(self.id.call)
        }
    }
}

/// An envelope on its way out: the recipients the route resolved, each
/// to be delivered `payload` under `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub id: EnvelopeId,
    pub recipients: Vec<SessionId>,
    pub payload: String,
}

impl Outgoing {
    /// The envelope recipient `to` is delivered.
    pub fn to(&self, to: SessionId) -> Envelope {
        Envelope {
            id: self.id.clone(),
            destination: to,
            payload: self.payload.clone(),
            held: Vec::new(),
        }
    }
}

/// Where a queued item comes from. A user's input is the queue's visible
/// part, which clients show and a user may withdraw; the rest is hidden,
/// shown at most as a status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    User,
    /// Another session's message.
    Agent(SessionId),
    /// The outcome of the session's own call `call`, which went on in the
    /// background: a detached call, or a process group it left.
    Job(CallId),
    /// The result of the child watched at this seq, no call waiting on it.
    Child(Seq),
}

impl Source {
    pub const fn visible(self) -> bool {
        matches!(self, Self::User)
    }
}

/// The output of call `call` of session `session` as it streams: what a
/// detached call's placeholder result names, and a background job's log.
/// Named by the session whose log holds the call, so a child or fork that
/// inherits the entry names the same stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutputStream {
    pub session: SessionId,
    pub call: CallId,
}

/// `stream:<call>`, as text names it without the store's path.
impl fmt::Display for OutputStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stream:{}", self.call)
    }
}

impl Outcome {
    /// The result a call detached as it ran is answered with at once:
    /// done, and marked so. Its stream is what the result holds.
    pub fn detached() -> Self {
        #[derive(Serialize)]
        struct Placeholder {
            detached: bool,
        }
        Self::Done {
            output: Json::encode(&Placeholder { detached: true }).unwrap_or_default(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InboxRefusal {
    Full,
    Unavailable,
    Unauthorized,
    Interrupted,
}
pub const INBOX_CAPACITY: usize = 128;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_accounting_and_frames_share_the_same_encoder() {
        let msg = ClientMsg::input("\"\n雪");
        let mut before = Vec::new();
        write_frame(&mut before, &msg).unwrap();
        let mut after = Vec::new();
        msg.write_frame(&mut after).unwrap();
        assert_eq!(before, after);
        assert_eq!(
            msg.retained_bytes(),
            size_of::<ClientMsg>() + after.len() - HEADER + 34
        );
        let msg = DaemonMsg::Error {
            message: "\"\n雪".into(),
        };
        let frame = Frame::new(&msg).unwrap();
        assert_eq!(
            msg.retained_bytes(),
            size_of::<DaemonMsg>() + frame.body().len()
        );
        let mut text = String::with_capacity(8192);
        text.push('x');
        assert!(ClientMsg::input(text).retained_bytes() >= 8192);
    }

    #[test]
    fn numbering_cannot_increase_a_mutations_admission_charge() {
        for mut msg in [
            ClientMsg::input("x"),
            ClientMsg::INTERRUPT,
            ClientMsg::approve(CallId::new(1).unwrap(), true),
        ] {
            let bytes = msg.retained_bytes();
            for seq in [0, 1, 10, u64::MAX] {
                *msg.client_seq_mut().unwrap() = Some(seq);
                assert_eq!(msg.retained_bytes(), bytes, "{msg:?}");
            }
        }
    }

    #[test]
    fn normal_admission_preserves_bytes_and_slots_for_control() {
        let budget = QueueBudget {
            bytes: 8192,
            requests: 3,
            control_reserve: 1,
        };
        assert!(budget.admits(4096, 1, false));
        assert!(!budget.admits(4097, 1, false));
        assert!(!budget.admits(1, 2, false));
        assert!(budget.admits(8192, 2, true));
        assert!(!budget.admits(8193, 2, true));
        assert!(!budget.admits(1, 3, true));
    }

    #[test]
    fn a_shared_frame_is_the_frame_written() {
        let entry = Entry::new(
            Seq::new(7).unwrap(),
            Epoch(0),
            Body::Assistant { text: "hi".into() },
        );
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
        claim.push(FrameKind::Json as u8);
        claim.extend_from_slice(br#"{"t":"list"}"#);
        let mut r = Counted(&claim, 0);
        let err = read_frame::<ClientMsg>(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(r.1 <= 128 * 1024, "asked for {} bytes at once", r.1);
    }

    /// Only the kinds this build knows are read: a binary frame from a
    /// newer peer is refused, never parsed as JSON.
    #[test]
    fn a_frame_of_an_unknown_kind_is_refused() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ClientMsg::Detach).unwrap();
        assert_eq!(buf[4], FrameKind::Json as u8);
        assert_eq!(frame_in(&buf).unwrap().map(|(end, _)| end), Some(buf.len()));
        buf[4] = 1;
        for err in [
            frame_in(&buf).unwrap_err(),
            read_frame::<ClientMsg>(&mut buf.as_slice()).unwrap_err(),
        ] {
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        }
        let empty = 0u32.to_le_bytes();
        assert!(frame_in(&empty).is_err());
        assert!(read_frame::<ClientMsg>(&mut empty.as_slice()).is_err());
    }

    #[test]
    fn frames_round_trip() {
        let msg = ClientMsg::input("hi");
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        write_frame(&mut buf, &ClientMsg::Detach).unwrap();
        let mut r = buf.as_slice();
        assert!(
            matches!(read_frame(&mut r).unwrap(), Some(ClientMsg::Input { segments: text, .. }) if text == "hi")
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
            Seq::new(1).unwrap(),
            Epoch(0),
            Body::Input {
                segments: "hi".into(),
                mode: Mode::Queue,
                taken: true,
                origin: None,
                force: false,
            },
        );
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"seq":1,"epoch":0,"kind":"input","segments":[{"text":"hi"}],"taken":true}"#
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
    fn a_settings_change_carries_only_what_differs() {
        let from = HomeSettings {
            model: Some("openai:a".into()),
            agent: None,
            env: [("KEEP", "1"), ("DROP", "2"), ("EDIT", "3")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        };
        let to = HomeSettings {
            model: Some(" openai:a ".into()),
            agent: Some("echo".into()),
            env: [("KEEP", "1"), ("EDIT", "4"), ("NEW", "5")]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        };
        let change = SettingsChange::between(&from, &to);
        assert_eq!((change.model, change.agent), (None, Some("echo".into())));
        assert_eq!(
            change.env,
            [("DROP", None), ("EDIT", Some("4")), ("NEW", Some("5"))]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.map(str::to_owned)))
                .collect()
        );
        assert_eq!(SettingsChange::between(&to, &to), SettingsChange::default());
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

    #[test]
    fn every_declared_tag_fits_its_decoder() {
        for tags in [
            Body::TAGS,
            WaitOn::TAGS,
            Verdict::TAGS,
            ClientMsg::TAGS,
            DaemonMsg::TAGS,
        ] {
            assert!(decode::Tag::fits(tags));
            for tag in tags {
                let decoded: decode::Tag = serde_json::from_value(serde_json::json!(tag)).unwrap();
                assert_eq!(decoded.as_str(), *tag);
            }
        }
        assert!(!decode::Tag::fits(&[""]));
        assert!(!decode::Tag::fits(&[
            "a_tag_longer_than_the_decoder_capacity"
        ]));
    }

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
        let entry = DaemonMsg::Entry(Entry::new(Seq::new(1).unwrap(), Epoch(0), Body::INTERRUPT));
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
                segments: "hi".into(),
                mode: Mode::Queue,
                taken: true,
                origin: Some(author),
                force: false,
            },
            Body::Input {
                segments: "also".into(),
                mode: Mode::Steer,
                taken: false,
                origin: None,
                force: false,
            },
            Body::Take {
                input: seq(2),
                into: None,
            },
            Body::ModelCall {
                request: None,
                model: "m".into(),
                attempt: 1,
                choice: ToolChoice::None,
                r#for: None,
                purpose: None,
            },
            Body::Decision {
                by: "policy".into(),
                call: CallId::new(4),
                d: Verdict::Permission {
                    permission: Permission::Ask(ApprovalReason::Mode { mode: "ask".into() }),
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
                value: Json::encode(&true).unwrap(),
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
            .map(|b| {
                serde_json::to_string(&Entry::new(Seq::new(5).unwrap(), Epoch(1), b.clone()))
                    .unwrap()
            })
            .collect();
        let who = r#""origin":{"client":"00000000-0000-0000-0000-00000000000c","seq":7}"#;
        let head = r#"{"seq":5,"epoch":1,"kind":"#;
        assert_eq!(
            json,
            [
                format!(r#"{head}"input","segments":[{{"text":"hi"}}],"taken":true,{who}}}"#),
                format!(r#"{head}"input","segments":[{{"text":"also"}}],"mode":"steer"}}"#),
                format!(r#"{head}"take","input":2}}"#),
                format!(r#"{head}"model_call","model":"m","attempt":1,"choice":"none"}}"#),
                format!(
                    r#"{head}"decision","by":"policy","call":4,"d":"permission","permission":{{"ask":{{"mode":{{"mode":"ask"}}}}}}}}"#
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

    /// An entry of a kind this build does not know reads whatever its
    /// fields hold, folds as nothing, and encodes as the text it was read
    /// from, byte for byte.
    #[test]
    fn unknown_entries_read_and_write_back_as_they_were() {
        for line in [
            r#"{"seq":4,"epoch":2,"kind":"newer","x":{"y":[1, 2]}}"#,
            r#"{"seq":4,"epoch":2,"kind":"newer","text":5,"call":"c","input":null}"#,
            r#"{"seq":4,"epoch":2,"kind":"a_kind_longer_than_any_tag_is"}"#,
        ] {
            let e = Entry::from_json(line.as_bytes()).unwrap();
            assert_eq!(
                (e.seq.get(), e.epoch.0, e.body.class()),
                (4, 2, Class::Durable)
            );
            let Body::Unknown { kind, raw } = &e.body else {
                panic!("{e:?}");
            };
            assert!(line.contains(&format!(r#""kind":"{kind}""#)));
            assert_eq!(raw.raw(), line);
            let mut written = Vec::new();
            e.encode(&mut written).unwrap();
            assert_eq!(written, line.as_bytes());
            let mut ledger = ledger::Ledger::default();
            ledger.apply(&e);
            assert_eq!(ledger.seq(), 4);
        }
        let frame =
            Frame::entry(&Entry::from_json(br#"{"seq":4,"epoch":2,"kind":"newer"}"#).unwrap());
        let msg = DaemonMsg::from_json(frame.unwrap().body()).unwrap();
        assert!(
            matches!(msg, DaemonMsg::Unknown { seq: Some(4) }),
            "{msg:?}"
        );
    }

    #[test]
    fn calls_without_cards_keep_their_shape_and_ext_entries_are_flat() {
        let line = r#"{"seq":3,"epoch":0,"kind":"tool_result","call":2,"outcome":{"status":"done","output":{"exit":0,"stderr":"","stdout":"hi\n"}}}"#;
        let e: Entry = serde_json::from_str(line).unwrap();
        assert!(matches!(&e.body, Body::ToolResult { card: None, .. }));
        assert_eq!(serde_json::to_string(&e).unwrap(), line);
        let carded = Body::ToolCall {
            invoke: None,
            tool: "bash".into(),
            input: Json::encode(&serde_json::json!({ "command": "ls" })).unwrap(),
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
            data: Json::encode("/w").unwrap(),
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
                call: CallId::new(2).unwrap(),
                text: "p\n".into(),
            },
            Body::ModelResult {
                r#for: None,
                failure: None,
                call: CallId::new(2).unwrap(),
                outcome: Outcome::Interrupted {
                    output: Some("so far".into()),
                },
            },
            Body::INTERRUPT,
        ];
        let json: Vec<_> = bodies
            .iter()
            .map(|b| {
                serde_json::to_string(&Entry::new(Seq::new(5).unwrap(), Epoch(1), b.clone()))
                    .unwrap()
            })
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
                intent: Json::null(),
                child,
                input: "hi".into(),
            },
            Body::Watch { child },
            Body::Fired {
                watch: Seq::new(2).unwrap(),
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
                format!(r#"{{"kind":"link","child":"{id}","input":"hi","intent":null}}"#),
                format!(r#"{{"kind":"watch","child":"{id}"}}"#),
                r#"{"kind":"fired","watch":2,"outcome":{"status":"interrupted"}}"#.to_owned(),
            ]
        );
    }
}
