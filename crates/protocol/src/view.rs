//! Shared reducers: the state every client (the CLI, the web UI, the apps
//! through their FFI facade) folds daemon messages into, so all of them show
//! the same session.
//!
//! The log holds the facts; everything here is derived from them. A
//! [`SessionView`]'s serialized form is its log-derived state: identical for
//! any two clients that have folded the same session up to the same seq,
//! which [`SessionView::state_hash`] checks. What only this connection saw
//! (streamed text, the recap of what it missed) is kept beside it and left
//! out. Kernel activity is also connection state: the daemon sends it after
//! the entries of a complete burst, including after replay or a snapshot.
//! Every new entry invalidates it until the next activity frame. This keeps
//! the gap between a reply and its requested calls from looking idle.
//! How an item is shown is decided once too, by [`Item::present`].

pub mod md;
mod present;

use std::borrow::{Borrow, Cow};
use std::collections::VecDeque;
use std::io;

use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::ledger::{Keyed, OutcomeKind, Phase};
use crate::{
    Activity, ApprovalReason, Audience, Body, CallId, Card, Carried, DaemonMsg, Entry, HostPath,
    Json, Outcome, Permission, Seq, SessionId, SessionSummary, ToolOutcome, Verdict, WaitOn,
};

pub use present::{
    Buttons, Glyph, Kind, Presented, Shown, answer_line, call_title, decision_line, mismatch_line,
    recap_line,
};

/// Reconnection and rendering state, with no authority to resume execution.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ViewSnapshot {
    pub session: Option<SessionId>,
    pub workspace: HostPath,
    pub seq: u64,
    pub state: ViewState,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct ViewState {
    calls: Keyed<CallId, ViewCall>,
    queued: VecDeque<Queued>,
    turn: u64,
    /// Transcript replacement must not erase logged input ownership.
    pub taken: Vec<crate::Author>,
    #[serde(
        default,
        skip_serializing_if = "crate::publication::Publications::is_empty"
    )]
    publications: crate::publication::Publications,
}

/// An item of the session's queue, as every client shows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Queued {
    pub seq: Seq,
    pub text: String,
    pub steer: bool,
    pub source: crate::Source,
    pub origin: Option<crate::Author>,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
struct ViewCall {
    independent: bool,
    question_seq: Option<Seq>,
    phase: Phase,
    progress: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    annotation: Option<(crate::AnnotationLifetime, Card)>,
}

impl ViewCall {
    fn discard_annotation(&mut self) {
        if !self.phase.asking()
            && self
                .annotation
                .as_ref()
                .is_some_and(|(lifetime, _)| *lifetime == crate::AnnotationLifetime::Question)
        {
            self.annotation = None;
        }
    }
}

impl ViewState {
    fn call(&self, call: CallId) -> Option<&ViewCall> {
        self.calls.get(call)
    }

    pub fn question_id(&self, call: CallId) -> Option<Seq> {
        let c = self.calls.get(call)?;
        c.phase.asking().then_some(c.question_seq?)
    }

    pub fn awaiting_permission(&self, call: CallId) -> bool {
        self.calls
            .get(call)
            .is_some_and(|c| matches!(c.phase, Phase::Waiting(WaitOn::Approval)))
    }

    pub fn question(&self, call: CallId) -> Option<(Seq, crate::Question)> {
        let c = self.calls.get(call)?;
        let question = match &c.phase {
            Phase::Waiting(WaitOn::Approval) => {
                crate::Question::confirm("Allow this tool call?".into())
            }
            Phase::Waiting(WaitOn::Question { request }) => request.question.clone(),
            _ => return None,
        };
        Some((c.question_seq?, question))
    }

    pub fn inbox(&self) -> impl Iterator<Item = CallId> + '_ {
        self.calls
            .iter()
            .filter(|(_, c)| c.phase.asking())
            .map(|(id, _)| id)
    }

    pub const fn turn_start(&self) -> u64 {
        self.turn
    }

    /// The queue, in order: its visible part ([`crate::Source::visible`])
    /// is the user's input, which a client may withdraw.
    pub fn queued(&self) -> impl Iterator<Item = &Queued> {
        self.queued.iter()
    }

    pub fn publications(&self) -> &crate::publication::Publications {
        &self.publications
    }

    #[allow(clippy::too_many_lines)]
    fn apply(&mut self, e: &Entry) {
        self.publications.apply(e);
        match &e.body {
            Body::Input {
                segments,
                mode,
                taken,
                origin,
                ..
            } => {
                if *taken {
                    self.taken.extend(origin);
                    self.turn = e.seq.get();
                } else {
                    self.queued.push_back(Queued {
                        seq: e.seq,
                        text: segments.text(),
                        steer: *mode == crate::Mode::Steer,
                        source: crate::Source::User,
                        origin: *origin,
                    });
                }
            }
            Body::Enqueue { envelope } => self.queued.push_back(Queued {
                seq: e.seq,
                // An outcome is hidden, shown as no text.
                text: match envelope.payload() {
                    Carried::Message(text) | Carried::Job(text) => text.clone(),
                    Carried::Child(_) | Carried::Detached(_) => String::new(),
                },
                steer: false,
                source: envelope.source(),
                origin: None,
            }),
            Body::Withdraw { input, .. } => self.queued.retain(|q| q.seq != *input),
            Body::Take { input, into } => {
                if let Some(at) = self.queued.iter().position(|q| q.seq == *input)
                    && let Some(q) = self.queued.remove(at)
                {
                    self.taken.extend(q.origin);
                    if !q.steer && into.is_none() {
                        self.turn = e.seq.get();
                    }
                }
            }
            Body::Question { question } => {
                self.calls.insert(
                    CallId(e.seq),
                    ViewCall {
                        independent: true,
                        question_seq: Some(e.seq),
                        phase: Phase::Waiting(WaitOn::Question {
                            request: Box::new(crate::QuestionWait {
                                question: question.clone(),
                                checkpoint: None,
                            }),
                        }),
                        progress: None,
                        annotation: None,
                    },
                );
            }
            Body::ToolCall { .. } | Body::ModelCall { .. } => {
                self.calls.insert(
                    CallId(e.seq),
                    ViewCall {
                        independent: false,
                        question_seq: None,
                        phase: if matches!(e.body, Body::ToolCall { .. }) {
                            Phase::Gating { at: 0 }
                        } else {
                            Phase::Running
                        },
                        progress: None,
                        annotation: None,
                    },
                );
            }
            Body::ToolResult { call, .. } | Body::ModelResult { call, .. } => {
                self.calls.remove(*call);
            }
            Body::Decision {
                call: Some(call),
                d,
                ..
            } => {
                if let Some(c) = self.calls.get_mut(*call) {
                    c.phase.decide(d);
                    if matches!(
                        d,
                        Verdict::Permission {
                            permission: Permission::Ask(_)
                        }
                    ) {
                        c.question_seq = Some(e.seq);
                    }
                    c.discard_annotation();
                }
            }
            Body::Wait { call, on } => {
                if let Some(c) = self.calls.get_mut(*call) {
                    let mut on = on.clone();
                    if let WaitOn::Question { request } = &mut on {
                        request.checkpoint = None;
                    }
                    c.phase.wait(&on);
                    c.question_seq = Some(e.seq);
                    c.discard_annotation();
                }
            }
            Body::Answer { call, value, .. } => {
                if self.calls.get(*call).is_some_and(|c| c.independent) {
                    self.calls.remove(*call);
                } else if let Some(c) = self.calls.get_mut(*call) {
                    c.phase.answer(value);
                    c.discard_annotation();
                }
            }
            Body::Fact {
                fact:
                    crate::ExtensionFact::Annotation(crate::Annotation {
                        target,
                        lifetime,
                        card,
                    }),
            } => {
                if let Some(c) = self.calls.get_mut(*target)
                    && (*lifetime != crate::AnnotationLifetime::Question || c.phase.asking())
                {
                    c.annotation = Some((*lifetime, card.clone()));
                }
            }
            Body::Progress { call, text } => {
                if let Some(c) = self.calls.get_mut(*call) {
                    c.progress.get_or_insert_default().push_str(text);
                    if matches!(c.phase, Phase::Gating { .. }) {
                        c.phase = Phase::Running;
                    }
                }
            }
            Body::ToolProcess { call, .. } => {
                if let Some(c) = self.calls.get_mut(*call)
                    && matches!(c.phase, Phase::Gating { .. })
                {
                    c.phase = Phase::Running;
                }
            }
            _ => {}
        }
    }
}

/// One session as a client shows it, including transient connection state.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SessionView {
    /// The kernel's activity at `seq`, or unknown while entries of its next
    /// burst arrive. Never inferred from an entry prefix: a reply may have
    /// settled before the calls it requested arrive. Attach sends this
    /// separately after replay or snapshot, so it is connection state.
    #[serde(skip)]
    pub activity: Option<Activity>,
    pub session: Option<SessionId>,
    pub workspace: HostPath,
    /// The last entry folded: a reconnect attaches with it as `since`.
    pub seq: u64,
    pub state: ViewState,
    /// The transcript, in log order.
    pub items: Vec<Item>,
    /// What happened since the seq the latest attach started from, once
    /// its replay is in.
    #[serde(skip)]
    pub recap: Option<Recap>,
    /// The seq the latest attach started from.
    #[serde(skip)]
    pub(crate) since: u64,
    /// Live text of the model call in flight; the committed reply replaces
    /// it.
    #[serde(skip)]
    pub streaming: Option<Stream>,
    /// Counts the updates [`SessionView::fold`] returned. An update whose
    /// `rev` is not one more than the last seen means some were dropped (an
    /// FFI stream's buffer was full): take a fresh snapshot of the view.
    #[serde(skip)]
    pub rev: u64,
}

/// What happened in a session, for a client that attaches: what its turn
/// did since the user's last message, as [`SessionView::recap_since`]
/// reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub call: CallId,
    /// The attempt the text is of: a retry streams anew.
    pub attempt: u16,
    pub text: String,
}

/// A transcript item, keyed by the seq of the entry that started it (for a
/// call, its call id).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    User {
        seq: Seq,
        text: String,
    },
    Assistant {
        seq: Seq,
        text: String,
    },
    /// A model call and the reply it got; a side request an extension
    /// made names its `purpose`.
    Reply {
        call: CallId,
        model: String,
        text: String,
        status: Status,
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        purpose: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<CallId>,
    },
    /// A call of a tool, by the model or (`by` the user) a slash command,
    /// whose arguments are its input's `args` ([`Item::args`]).
    Tool {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invoke: Option<crate::Invocation>,
        call: CallId,
        tool: String,
        input: Json,
        /// Where it stands, as the ledger has it: once it has its outcome,
        /// settled with that outcome's kind.
        phase: Phase,
        outcome: Option<ToolOutcome>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<Box<ApprovalReason>>,
        /// How the tool presents the call, or once it has one, its outcome.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        card: Option<Card>,
        #[serde(default, skip_serializing_if = "crate::decode::skip")]
        by: Audience,
    },
    /// What extension `hook` decided, about call `call` (of tool `tool`)
    /// when it is about one.
    Hook {
        seq: Seq,
        hook: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<CallId>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tool: String,
        decision: Verdict,
    },
    /// A child session started by call `call`, and its watch.
    Child {
        call: CallId,
        child: SessionId,
        input: String,
        watch: Option<Seq>,
        fired: Option<ToolOutcome>,
    },
    Environment {
        seq: Seq,
        cwd: HostPath,
    },
}

impl Item {
    /// Where the call this item shows stands; `None` for an item that is
    /// not a call.
    pub fn phase(&self) -> Option<Phase> {
        Some(match self {
            Self::Reply { status, .. } => match status {
                Status::Running => Phase::Running,
                Status::Done => Phase::Settled(OutcomeKind::Done),
                Status::Failed { .. } => Phase::Settled(OutcomeKind::Failed),
                Status::Interrupted => Phase::Settled(OutcomeKind::Interrupted),
            },
            Self::Tool { phase, .. } => phase.clone(),
            _ => return None,
        })
    }

    /// Whether the item will not change any more: a call with its outcome,
    /// or no call at all.
    pub const fn settled(&self) -> bool {
        match self {
            Self::Reply { status, .. } => !matches!(status, Status::Running),
            Self::Tool { outcome, .. } => outcome.is_some(),
            _ => true,
        }
    }

    /// The seq of the entry that started this item.
    pub const fn key(&self) -> Seq {
        match self {
            Self::User { seq, .. }
            | Self::Assistant { seq, .. }
            | Self::Hook { seq, .. }
            | Self::Environment { seq, .. } => *seq,
            Self::Reply { call, .. } | Self::Tool { call, .. } | Self::Child { call, .. } => call.0,
        }
    }

    /// The arguments of a slash command, as typed after its name: its
    /// input's `args`.
    pub fn args(input: &Json) -> String {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            args: String,
        }
        input.parse::<Args>().map_or_default(|a| a.args)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Running,
    Done,
    Failed { error: String },
    Interrupted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}

/// `12 in · 34 out`.
impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} in · {} out", self.input, self.output)
    }
}

/// What one daemon message changed, as a client applies it to its own
/// copy: an item added is carried; one updated is read back from the view
/// by index.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Update {
    /// One more than the previous update's: a gap means updates were lost.
    pub rev: u64,
    /// The view's seq after this update.
    pub seq: u64,
    pub change: Change,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum Change {
    /// Attached to `session`; `reset` when it is not the one shown before,
    /// so the view started over.
    Attached {
        session: SessionId,
        workspace: HostPath,
        reset: bool,
    },
    Added {
        index: usize,
        item: Item,
    },
    /// One fact changed several items, applied together.
    Batch {
        changes: Vec<Change>,
    },
    Updated {
        index: usize,
        part: Part,
    },
    /// Live text for the reply of `call`, appended to [`SessionView::streaming`].
    Delta {
        call: CallId,
        text: String,
    },
    /// The replay is in; [`SessionView::recap`] tells what it held.
    Synced,
    /// The kernel published activity for the entries folded so far.
    Activity,
    /// The daemon sent the whole view: the items are these now.
    Snapshot,
    Draining,
    Mismatch {
        supported: u32,
    },
    Error {
        message: String,
    },
}

/// Which part of an item an [`Change::Updated`] changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "part", rename_all = "snake_case")]
pub enum Part {
    /// A tool's result, a reply's, a command's, or a child's fired watch.
    Outcome,
    /// Where the call stands changed: it is asked about, runs, or was
    /// refused.
    Phase,
    /// More output was committed by a running tool.
    Progress,
    /// Optional explanatory text arrived independently of the question.
    Presentation,
    /// The approval the call waited for was given, or not.
    Answer {
        approved: bool,
    },
    Watch,
}

impl SessionView {
    pub fn snapshot(&self) -> ViewSnapshot {
        ViewSnapshot {
            session: self.session,
            workspace: self.workspace.clone(),
            seq: self.seq,
            state: self.state.clone(),
            items: self.items.clone(),
        }
    }

    /// The view of `session` as its log `entries` leave it: what a client
    /// that attached and folded them would hold, but for what only a
    /// connection sees.
    pub fn from_log(
        session: SessionId,
        workspace: HostPath,
        entries: impl IntoIterator<Item = impl Borrow<Entry>>,
    ) -> Self {
        let mut view = Self {
            session: Some(session),
            workspace,
            ..Self::default()
        };
        view.extend(entries);
        view
    }

    /// Folds `entries`, committed after those folded so far: how a daemon
    /// keeps the view a far attach is sent.
    pub fn extend(&mut self, entries: impl IntoIterator<Item = impl Borrow<Entry>>) {
        for e in entries {
            self.entry(e.borrow());
        }
    }

    /// Folds one daemon message in. `None` when it changed nothing, such as
    /// an entry replayed again after a reconnect, or a kind this build does
    /// not know.
    pub fn fold(&mut self, msg: &DaemonMsg) -> Option<Update> {
        let change = self.apply(msg)?;
        self.rev += 1;
        Some(Update {
            rev: self.rev,
            seq: self.seq,
            change,
        })
    }

    /// A call's live output comes from the ledger until its final outcome
    /// replaces it, so attaching mid-call shows the same output as watching.
    pub fn present<'a>(&'a self, item: &'a Item) -> Presented<'a> {
        let mut shown = item.present();
        shown.question_seq = self
            .state
            .call(CallId(item.key()))
            .and_then(|c| c.question_seq);
        if let Item::Tool {
            call,
            outcome: None,
            ..
        } = item
            && let Some(text) = self.state.call(*call).and_then(|c| c.progress.as_deref())
        {
            shown.body = Shown::Text(text.into());
        }
        if let Some((lifetime, card)) = self
            .state
            .call(CallId(item.key()))
            .and_then(|c| c.annotation.as_ref())
        {
            match lifetime {
                crate::AnnotationLifetime::Question => shown.description = Some(&card.body),
                crate::AnnotationLifetime::Call => {
                    if !card.title.is_empty() {
                        shown.title = card.title.as_str().into();
                    }
                    shown.body = Shown::Text(card.body.as_str().into());
                    if !card.note.is_empty() {
                        shown.status = card.note.as_str().into();
                    }
                    shown.glyph = Glyph::of(card.mark);
                }
            }
        }
        shown
    }

    /// The latest done reply to a side request for `purpose` (a title,
    /// say), trimmed and bare of the quotes a model wraps a short answer
    /// in; `None` until there is one.
    pub fn side(&self, purpose: &str) -> Option<&str> {
        self.items.iter().rev().find_map(|item| match item {
            Item::Reply {
                text,
                status: Status::Done,
                purpose: Some(p),
                ..
            } if p == purpose => {
                Some(text.trim().trim_matches('"').trim()).filter(|t| !t.is_empty())
            }
            _ => None,
        })
    }

    /// A hash of the log-derived state (the serialized view): equal on every
    /// client that folded the same session to the same seq. FNV-1a, so it is
    /// the same on every platform and build.
    pub fn state_hash(&self) -> u64 {
        let mut hash = Fnv(0xcbf2_9ce4_8422_2325);
        serde_json::to_writer(&mut hash, self).expect("a view always serializes");
        hash.0
    }

    fn apply(&mut self, msg: &DaemonMsg) -> Option<Change> {
        match msg {
            DaemonMsg::Attached { session, workspace } => {
                let reset = self.session != Some(*session);
                if reset {
                    *self = Self {
                        session: Some(*session),
                        rev: self.rev,
                        ..Self::default()
                    };
                }
                self.activity = None;
                self.since = self.seq;
                self.recap = None;
                self.workspace.clone_from(workspace);
                Some(Change::Attached {
                    session: *session,
                    workspace: workspace.clone(),
                    reset,
                })
            }
            DaemonMsg::Entry(e) => self.entry(e),
            DaemonMsg::Activity { seq, activity } if *seq == self.seq => {
                self.activity = Some(*activity);
                Some(Change::Activity)
            }
            DaemonMsg::Delta {
                call,
                text,
                attempt,
            } => {
                match &mut self.streaming {
                    Some(s) if s.call == *call && s.attempt == *attempt => s.text.push_str(text),
                    s => {
                        *s = Some(Stream {
                            call: *call,
                            attempt: *attempt,
                            text: text.clone(),
                        });
                    }
                }
                Some(Change::Delta {
                    call: *call,
                    text: text.clone(),
                })
            }
            DaemonMsg::Synced { seq } => {
                self.seq = self.seq.max(*seq);
                self.recap = self.recap_since(self.since);
                Some(Change::Synced)
            }
            DaemonMsg::Draining { .. } => Some(Change::Draining),
            DaemonMsg::Mismatch { supported, .. } => Some(Change::Mismatch {
                supported: *supported,
            }),
            DaemonMsg::Error { message } => Some(Change::Error {
                message: message.clone(),
            }),
            DaemonMsg::Rejected {
                reason, message, ..
            } => Some(Change::Error {
                message: if message.is_empty() {
                    format!("refused: {reason:?}")
                } else {
                    message.clone()
                },
            }),
            DaemonMsg::Snapshot(view) => Some(self.adopt(view)),
            // Skipped, but a reconnect still starts after it.
            DaemonMsg::Unknown { seq: Some(seq) } => {
                if *seq > self.seq {
                    self.activity = None;
                    self.seq = *seq;
                }
                None
            }
            DaemonMsg::Activity { .. }
            | DaemonMsg::Welcome { .. }
            | DaemonMsg::ForkRejected { .. }
            | DaemonMsg::Sessions { .. }
            | DaemonMsg::Settings { .. }
            | DaemonMsg::Status { .. }
            | DaemonMsg::Ack { .. }
            | DaemonMsg::Update { .. }
            | DaemonMsg::Reply { .. }
            | DaemonMsg::NotHeld { .. }
            | DaemonMsg::Unknown { .. } => None,
        }
    }

    /// Takes the log-derived state of `snapshot`, keeping what only this
    /// connection saw where it still holds: the reply streaming in.
    fn adopt(&mut self, snapshot: &ViewSnapshot) -> Change {
        let running = |call| {
            snapshot.items.iter().any(
                |i| matches!(i, Item::Reply { call: c, status: Status::Running, .. } if *c == call),
            )
        };
        if self.streaming.as_ref().is_some_and(|s| !running(s.call)) {
            self.streaming = None;
        }
        self.activity = None;
        self.session = snapshot.session;
        self.workspace.clone_from(&snapshot.workspace);
        self.seq = snapshot.seq;
        self.state.clone_from(&snapshot.state);
        self.items.clone_from(&snapshot.items);
        Change::Snapshot
    }

    /// Folds a new entry: most start an item, the rest change one.
    #[allow(clippy::too_many_lines)]
    fn entry(&mut self, e: &Entry) -> Option<Change> {
        if e.seq.get() <= self.seq {
            return None;
        }
        self.activity = None;
        self.seq = e.seq.get();
        let seq = e.seq;
        // Input taken up is shown as the user's message where it was taken:
        // its text is in the ledger until this entry leaves it. What the
        // queue hid stays hidden.
        let taken = if let Body::Take { input, .. } = &e.body {
            self.state
                .queued
                .iter()
                .find(|q| q.seq == *input && q.source.visible())
                .map(|q| q.text.clone())
        } else {
            None
        };
        self.state.apply(e);
        let item = match &e.body {
            Body::Input {
                segments,
                taken: true,
                ..
            } => Item::User {
                seq,
                text: segments.text(),
            },
            Body::Input { .. } => return None,
            Body::Take { .. } => Item::User { seq, text: taken? },
            Body::ToolCall {
                tool,
                input,
                by: Audience::User,
                invoke: None,
                ..
            } => return Some(self.command(seq, tool, input)),
            Body::Decision {
                call: Some(call),
                d:
                    Verdict::Permission {
                        permission: Permission::Ask(reason),
                    },
                ..
            } => return self.asked(*call, reason),
            Body::Decision { by, call, d } => return Some(self.decision(seq, by, *call, d)),
            Body::Assistant { text } => Item::Assistant {
                seq,
                text: text.clone(),
            },
            Body::ToolCall {
                tool,
                input,
                card,
                by,
                invoke,
                ..
            } => Item::Tool {
                invoke: invoke.clone(),
                call: CallId(seq),
                tool: tool.clone(),
                input: input.clone(),
                phase: Phase::Gating { at: 0 },
                outcome: None,
                reason: None,
                card: card.clone(),
                by: *by,
            },
            Body::ModelCall {
                model,
                purpose,
                r#for,
                ..
            } => Item::Reply {
                call: CallId(seq),
                model: model.clone(),
                text: String::new(),
                status: Status::Running,
                usage: None,
                purpose: purpose.clone(),
                owner: *r#for,
            },
            Body::Link { child, input, .. } => Item::Child {
                call: CallId(seq),
                child: *child,
                input: input.clone(),
                watch: None,
                fired: None,
            },
            Body::Question { question } => Item::Tool {
                invoke: None,
                call: CallId(seq),
                tool: question.source.clone(),
                input: Json::null(),
                phase: Phase::Waiting(WaitOn::Question {
                    request: Box::new(crate::QuestionWait {
                        question: question.clone(),
                        checkpoint: None,
                    }),
                }),
                outcome: None,
                reason: None,
                card: None,
                by: Audience::Plugin,
            },
            Body::Environment(env) => Item::Environment {
                seq,
                cwd: env.cwd.clone(),
            },
            body => return self.change(seq, body),
        };
        self.items.push(item.clone());
        Some(Change::Added {
            index: self.items.len() - 1,
            item,
        })
    }

    /// Folds entry `seq`, which changes an item already shown.
    fn change(&mut self, seq: Seq, body: &Body) -> Option<Change> {
        match body {
            Body::Fact {
                fact: crate::ExtensionFact::Annotation(annotation),
            } => {
                self.state.call(annotation.target)?.annotation.as_ref()?;
                Some(Change::Updated {
                    index: self.call(annotation.target)?,
                    part: Part::Presentation,
                })
            }
            Body::ToolResult {
                call,
                outcome,
                card,
                ..
            } => {
                let at = self.call(*call)?;
                let Item::Tool {
                    outcome: o,
                    card: shown,
                    phase,
                    ..
                } = &mut self.items[at]
                else {
                    return None;
                };
                *phase = Phase::Settled(OutcomeKind::of(outcome));
                *o = Some(outcome.clone());
                if card.is_some() {
                    shown.clone_from(card);
                }
                Some(Change::Updated {
                    index: at,
                    part: Part::Outcome,
                })
            }
            Body::ModelResult { call, outcome, .. } => self.replied(*call, outcome),
            Body::Answer { call, .. } => {
                let at = self.call(*call)?;
                let Item::Tool { phase, .. } = &mut self.items[at] else {
                    return None;
                };
                let now = self.state.call(*call).map(|open| &open.phase);
                match (&*phase, now) {
                    (Phase::Waiting(WaitOn::Question { .. }), None) => {
                        *phase = Phase::Settled(OutcomeKind::Done);
                        Some(Change::Updated {
                            index: at,
                            part: Part::Phase,
                        })
                    }
                    (Phase::Waiting(WaitOn::Approval), Some(now)) if !now.asking() => {
                        let approved = *now == Phase::Running;
                        *phase = now.clone();
                        Some(Change::Updated {
                            index: at,
                            part: Part::Answer { approved },
                        })
                    }
                    _ => self.rephase(*call),
                }
            }
            Body::Progress { call, .. } => {
                self.rephase(*call);
                Some(Change::Updated {
                    index: self.call(*call)?,
                    part: Part::Progress,
                })
            }
            Body::Wait { call, .. } | Body::ToolProcess { call, .. } => self.rephase(*call),
            Body::Watch { child } => {
                let at = self.items.iter().rposition(
                    |item| matches!(item, Item::Child { child: c, watch: None, .. } if c == child),
                )?;
                if let Item::Child { watch, .. } = &mut self.items[at] {
                    *watch = Some(seq);
                }
                Some(Change::Updated {
                    index: at,
                    part: Part::Watch,
                })
            }
            Body::Fired { watch, outcome } => {
                let at = self.items.iter().rposition(
                    |item| matches!(item, Item::Child { watch: Some(w), .. } if w == watch),
                )?;
                if let Item::Child { fired, .. } = &mut self.items[at] {
                    *fired = Some(outcome.clone());
                }
                Some(Change::Updated {
                    index: at,
                    part: Part::Outcome,
                })
            }
            _ => None,
        }
    }

    fn decision(&mut self, seq: Seq, by: &str, call: Option<CallId>, d: &Verdict) -> Change {
        let mut changed = call.and_then(|call| self.rephase(call));
        if let Some(index) = call.and_then(|call| self.call(call))
            && let Verdict::PreTool { input } = d
            && let Item::Tool {
                input: effective, ..
            } = &mut self.items[index]
        {
            effective.clone_from(input);
            changed = Some(Change::Updated {
                index,
                part: Part::Presentation,
            });
        }
        let item = Item::Hook {
            seq,
            hook: by.to_owned(),
            call,
            tool: self.tool_of(call),
            decision: d.clone(),
        };
        self.items.push(item.clone());
        let added = Change::Added {
            index: self.items.len() - 1,
            item,
        };
        match changed {
            Some(changed) => Change::Batch {
                changes: vec![changed, added],
            },
            None => added,
        }
    }

    fn asked(&mut self, call: CallId, reason: &ApprovalReason) -> Option<Change> {
        let at = self.call(call)?;
        if let Item::Tool { reason: shown, .. } = &mut self.items[at] {
            *shown = Some(Box::new(reason.clone()));
        }
        self.rephase(call)
    }

    /// Brings tool call `call`'s item to the phase the ledger has it at;
    /// the change, if that moved it.
    fn rephase(&mut self, call: CallId) -> Option<Change> {
        let now = self.state.call(call)?.phase.clone();
        let at = self.call(call)?;
        match &mut self.items[at] {
            Item::Tool { phase, .. } if *phase != now => *phase = now,
            _ => return None,
        }
        Some(Change::Updated {
            index: at,
            part: Part::Phase,
        })
    }

    /// A command the user ran, called `seq`. The input that ran it is
    /// shown as the command it is.
    fn command(&mut self, seq: Seq, tool: &str, input: &Json) -> Change {
        let item = Item::Tool {
            invoke: None,
            call: CallId(seq),
            tool: tool.to_owned(),
            input: input.clone(),
            phase: Phase::Gating { at: 0 },
            outcome: None,
            reason: None,
            card: None,
            by: Audience::User,
        };
        let typed = |text: &str| {
            let rest = text.strip_prefix('/').unwrap_or_default();
            rest.split_whitespace().next() == Some(tool)
        };
        if let Some(Item::User { text, .. }) = self.items.last()
            && typed(text)
        {
            let at = self.items.len() - 1;
            self.items[at] = item;
            return Change::Updated {
                index: at,
                part: Part::Outcome,
            };
        }
        self.items.push(item.clone());
        Change::Added {
            index: self.items.len() - 1,
            item,
        }
    }

    /// The tool of call `call`, as a hook item names it.
    fn tool_of(&self, call: Option<CallId>) -> String {
        match call
            .and_then(|call| self.call(call))
            .map(|at| &self.items[at])
        {
            Some(Item::Tool { tool, .. }) => tool.clone(),
            _ => String::new(),
        }
    }

    /// What the replay after `since` held, as a recap tells it: the user's
    /// last message and what the turn did since, `None` when nothing worth
    /// telling is new (an environment, say).
    pub fn recap_since(&self, since: u64) -> Option<Recap> {
        if self.seq <= since {
            return None;
        }
        let prompt = self
            .items
            .iter()
            .rposition(|i| matches!(i, Item::User { .. }));
        let mut recap = Recap {
            prompt: prompt.and_then(|at| match &self.items[at] {
                Item::User { text, .. } => Some(short(text)),
                _ => None,
            }),
            asks: u32::try_from(self.state.inbox().count()).unwrap_or(u32::MAX),
            ..Recap::default()
        };
        for item in &self.items[prompt.map_or(0, |at| at + 1)..] {
            match item {
                Item::Tool {
                    tool,
                    by: Audience::Model,
                    ..
                } => recap.tools.push(tool.clone()),
                Item::Assistant { text, .. } => recap.reply = Some(short(text)),
                Item::Reply {
                    text,
                    status: Status::Done,
                    purpose: None,
                    ..
                } if !text.is_empty() => recap.reply = Some(short(text)),
                _ => {}
            }
            match item.phase() {
                Some(Phase::Settled(OutcomeKind::Failed | OutcomeKind::Interrupted)) => {
                    recap.failed += 1;
                }
                Some(Phase::Settled(OutcomeKind::Done)) | None => {}
                Some(phase) if !phase.asking() => recap.running += 1,
                Some(_) => {}
            }
        }
        (recap != Recap::default()).then_some(recap)
    }

    fn replied(&mut self, call: CallId, outcome: &Outcome) -> Option<Change> {
        if self.streaming.as_ref().is_some_and(|s| s.call == call) {
            self.streaming = None;
        }
        let at = self.call(call)?;
        let Item::Reply {
            text,
            status,
            usage,
            ..
        } = &mut self.items[at]
        else {
            return None;
        };
        *status = match outcome {
            Outcome::Done { output } => {
                let reply = output.parse::<Reply>().unwrap_or_default();
                *usage = reply.usage.and_then(ReplyUsage::counts);
                *text = reply.message.content.0;
                Status::Done
            }
            Outcome::Failed { error } => Status::Failed {
                error: error.clone(),
            },
            Outcome::Interrupted { output } => {
                *text = output.clone().unwrap_or_default();
                Status::Interrupted
            }
        };
        Some(Change::Updated {
            index: at,
            part: Part::Outcome,
        })
    }

    /// The index of the item call `call` started.
    fn call(&self, call: CallId) -> Option<usize> {
        self.items.binary_search_by_key(&call.0, Item::key).ok()
    }
}

/// What the view reads of a stored model reply, straight from its text:
/// its message's text and its token counts. The rest is skipped unread.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Reply {
    message: ReplyMessage,
    usage: Option<ReplyUsage>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ReplyMessage {
    content: Texts,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ReplyUsage {
    input: Option<u64>,
    output: Option<u64>,
}

impl ReplyUsage {
    fn counts(self) -> Option<Usage> {
        Some(Usage {
            input: self.input?,
            output: self.output?,
        })
    }
}

/// A message's text parts, joined as they are read: no part is kept.
#[derive(Default)]
struct Texts(String);

impl<'de> Deserialize<'de> for Texts {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Part<'a> {
            #[serde(rename = "type", borrow)]
            kind: Cow<'a, str>,
            #[serde(default, borrow)]
            text: Option<Cow<'a, str>>,
        }

        struct Join;

        impl<'de> Visitor<'de> for Join {
            type Value = Texts;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a message's parts")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut parts: A) -> Result<Texts, A::Error> {
                let mut text = String::new();
                while let Some(part) = parts.next_element::<Part<'de>>()? {
                    if let ("text", Some(more)) = (&*part.kind, part.text) {
                        text.push_str(&more);
                    }
                }
                Ok(Texts(text))
            }
        }

        d.deserialize_seq(Join)
    }
}

/// The first line of `text`, cut to a recap's width.
fn short(text: &str) -> String {
    const WIDTH: usize = 60;
    let text = text.trim();
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(WIDTH) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None if line.len() < text.len() => format!("{line}…"),
        None => line.to_owned(),
    }
}

/// The command a call's input runs, when it runs one: its `command`.
pub fn command(input: &Json) -> Option<Cow<'_, str>> {
    #[derive(Deserialize)]
    struct Command<'a> {
        #[serde(borrow)]
        command: Cow<'a, str>,
    }
    input.parse::<Command<'_>>().ok().map(|c| c.command)
}

/// The daemon's sessions, newest first, as `a ls` and a session picker
/// show them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
}

impl SessionList {
    /// Whether `msg` replaced the list.
    pub fn fold(&mut self, msg: &DaemonMsg) -> bool {
        match msg {
            DaemonMsg::Sessions { sessions } => {
                self.sessions.clone_from(sessions);
                true
            }
            _ => false,
        }
    }
}

struct Fnv(u64);

impl io::Write for Fnv {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod question_tests {
    use super::*;

    #[test]
    fn answering_a_service_question_releases_its_live_tracking() {
        let mut view = SessionView::default();
        let id = Seq::new(1).unwrap();
        view.fold(&DaemonMsg::Entry(Entry::new(
            id,
            crate::Epoch(1),
            Body::Question {
                question: crate::Question::confirm("Continue?".into()),
            },
        )));
        assert!(view.state.calls.iter().next().is_some());
        view.fold(&DaemonMsg::Entry(Entry::new(
            Seq::new(2).unwrap(),
            crate::Epoch(1),
            Body::Answer {
                call: CallId(id),
                value: Json::encode(&crate::QuestionAnswer::confirm(
                    id,
                    crate::Response::accept(),
                ))
                .unwrap(),
                origin: None,
            },
        )));
        assert!(view.state.calls.iter().next().is_none());
        assert!(matches!(
            view.items[0],
            Item::Tool {
                phase: Phase::Settled(_),
                ..
            }
        ));
    }
}
