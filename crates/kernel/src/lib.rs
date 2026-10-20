//! The session kernel: what a session does next, as a pure function of
//! what it has logged and what just happened.
//!
//! [`Core::step`] takes one [`Event`] (input, an effect's outcome, a timer
//! firing) and returns the [`Effects`] that follow: entries to append, and
//! the work they are the committed intent of. It is sync, does no I/O and
//! reads no clock (time comes in as events), so every ordering a session
//! can meet is a row of a table test, and two runs of the same events log
//! the same entries. The driver (the engine) carries the effects out and
//! feeds their outcomes back.
//!
//! The kernel numbers the entries it appends and folds each at once, as it
//! folds an entry it loads: the [`Ledger`](protocol::ledger::Ledger), and the agent, whose inputs are
//! all logged facts. So the state a live session reaches and the state
//! loading its log rebuilds are one fold, and recovery is one function,
//! [`Core::resume`]: after a load, a failed commit, a registry reload or a
//! panic alike.
//!
//! The kernel ships no tool, prompt, permission rule, model choice, retry
//! policy or agent loop: all of that answers through the session's
//! [`Registry`] and [`Agent`].

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

pub use ext::{
    Action, Actions, Agent, Answer, Callable, Chained, Concurrency, Gate, Gated, Input, Later,
    LoopPoints, Name, Parsed, Recover, Replay,
};
use protocol::{
    Author, CallId, Card, Entry, Environment, Installed, Json, Mode, Outcome, Seq, SessionId,
    ToolChoice, WaitOn,
};
use smallvec::SmallVec;

mod fold;
mod session;

pub use crate::session::{Core, RESTART_CAP};

/// What a step returns, in order. The driver commits each run of
/// [`Effect::Append`]s before the effect after it starts, and the last run
/// at the end, so an intent is durable before the work it names (and one
/// step's entries share an fsync). If a commit fails, the effects after it
/// are dropped and the driver resumes from the log ([`Core::resume`]).
pub type Effects = SmallVec<[Effect; 4]>;

/// What happened, as the driver tells the kernel.
#[derive(Debug)]
pub enum Event {
    /// Input from a client: started at once with nothing to wait for,
    /// otherwise queued. A `Steer` goes to the running turn before its next
    /// model call.
    Input {
        text: String,
        mode: Mode,
        origin: Option<Author>,
        ticket: Option<Ticket>,
    },
    /// Stop the running turn. With none running it stops nothing.
    Interrupt {
        origin: Option<Author>,
        ticket: Option<Ticket>,
    },
    /// The answer to what call `call` waits on: a user's approval
    /// (`true`/`false`), a dialog's value.
    Answer {
        call: CallId,
        value: Json,
        origin: Option<Author>,
        ticket: Option<Ticket>,
    },
    /// Model call `call` ended.
    ModelDone { call: CallId, replied: Replied },
    /// Tool call `call` ended, its outcome shown as `card`.
    ToolDone {
        call: CallId,
        outcome: Outcome,
        card: Option<Card>,
    },
    /// Tool call `call`, running, reports something to log.
    Note { call: CallId, note: Note },
    /// Tool call `call`, running, waits on a dialog or a client.
    Waits { call: CallId, on: WaitOn },
    /// Tool call `call`, running, starts child session `child` on `input`
    /// and watches it; with `wait`, the call waits for its result.
    Spawn {
        call: CallId,
        child: SessionId,
        input: String,
        wait: bool,
    },
    /// The gate of call `call` answered later ([`Answer::Later`]).
    Gated { call: CallId, gated: Gated },
    /// The backoff before retrying model call `call` is over.
    Woke { call: CallId },
    /// How to recover model call `call`, answered later
    /// ([`Effect::Recover`]): `None` fails it.
    Recovered {
        call: CallId,
        recovered: Option<(Name, Recover)>,
    },
    /// An extension asks `model` aside on behalf of open call `of`, for
    /// `purpose`: logged as a model call of its own under it, its id the
    /// answer to `ticket` ([`Effect::Side`]).
    Side {
        of: CallId,
        purpose: String,
        model: String,
        ticket: Ticket,
    },
    /// Side request `side` ended.
    SideDone { side: CallId, outcome: Outcome },
    /// The `step` point decided `decisions` about model call `call`'s
    /// request as it was sent (or answered it).
    Prepared {
        call: CallId,
        decisions: ext::host::Decided,
    },
    /// Child session `child` settled with `outcome`.
    ChildSettled { child: SessionId, outcome: Outcome },
    /// A new registry generation, taken up at the next step boundary.
    Registry(Arc<dyn Registry>),
    /// The environment the session's commands run in from now on, as
    /// logged (secrets redacted): logged unless it is the one last logged.
    Environment(Environment),
}

/// How a model call ended.
#[derive(Debug)]
pub enum Replied {
    /// The whole reply, as the agent reads it.
    Done(Json),
    Failed(llm::Error),
    /// Cut off, with the text it had streamed.
    Interrupted(Option<String>),
}

/// What a running tool reports, logged under its call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// It started process group `pgid` in boot `boot`.
    Process {
        pgid: u32,
        boot: String,
        background: bool,
    },
    /// Output since its last progress.
    Progress(String),
    /// A change to extension `ext`'s durable state `state`.
    State {
        ext: String,
        state: String,
        data: Json,
    },
    /// What the model sees from here on: entries `target..=upto` stand as
    /// `replace`, or are left out.
    Edit {
        target: Seq,
        upto: Option<Seq>,
        replace: Option<Json>,
    },
}

/// A client's request, numbered by the driver so the reply finds it. Never
/// logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ticket(pub u64);

/// What the driver is to do.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Commit this entry, numbered next.
    Append(Entry),
    /// Answer request `ticket`, once what it logged is committed.
    Reply {
        ticket: Ticket,
        result: Result<(), String>,
    },
    /// Answer side request `ticket` with its call's id, once committed;
    /// `None`, it may not be made (its owner is not open).
    Side {
        ticket: Ticket,
        side: Option<CallId>,
    },
    /// Ask the model to reply to the transcript before `call`, as it is
    /// now, as attempt `attempt` (from 0), streaming under it, with the
    /// tools `choice` lets it call.
    CallModel {
        call: CallId,
        attempt: u16,
        choice: ToolChoice,
    },
    /// Run tool call `call`: on `input`, read already as `parsed` unless
    /// it is to be read now.
    RunTool {
        call: CallId,
        tool: String,
        input: Json,
        parsed: Option<Parsed>,
    },
    /// Hand tool call `call` the answer to what it waited on.
    Resume {
        call: CallId,
        tool: String,
        input: Json,
        value: Json,
    },
    /// Await the gate's answer for call `call`; it comes back as
    /// [`Event::Gated`]. Dropped by a [`Effect::Cancel`] of the call.
    Await { call: CallId, answer: Later<Gated> },
    /// Await how to recover model call `call`, which a `request_error`
    /// handler answers later; it comes back as [`Event::Recovered`].
    /// Dropped by a [`Effect::Cancel`] of the call.
    Recover {
        call: CallId,
        answer: Later<Option<(Name, Recover)>>,
    },
    /// Wake the session with [`Event::Woke`] after `after`.
    Sleep { call: CallId, after: Duration },
    /// Stop what runs, or sleeps, for call `call`; it reports as it ends.
    Cancel { call: CallId },
    /// Start child session `child` on `input`, owned by link `link`,
    /// unless it runs already.
    StartChild {
        link: Seq,
        child: SessionId,
        input: String,
    },
    /// Interrupt child session `child`'s turn.
    Interrupt { child: SessionId },
    /// Hand this child session's result to its owner.
    Report { outcome: Outcome },
    /// Kill process group `pgid`, left over from a run cut off.
    Kill { pgid: u32 },
    /// Whether the session has work in flight, which a restart recovers.
    MarkBusy(bool),
}

/// Where the session's turn stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Turn {
    /// Nothing runs: input starts a turn at once.
    #[default]
    Idle,
    /// A model call is out, as attempt `attempt`, or waits to be sent
    /// again as it.
    Model {
        call: CallId,
        attempt: u16,
        phase: ModelPhase,
    },
    /// The open tool calls of the turn: shared calls of one reply run
    /// together, and the agent's next action waits until none is open.
    Acting { open: SmallVec<[(CallId, Step); 2]> },
    /// Interrupted: once these calls settle, the turn ends.
    Stopping {
        open: SmallVec<[CallId; 2]>,
        origin: Option<Author>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelPhase {
    Streaming,
    /// Its last attempt failed; it is sent again once [`Event::Woke`].
    Backoff,
    /// Its last attempt failed, and a `request_error` handler answers
    /// later how to recover ([`Event::Recovered`]).
    Recovering,
}

/// Where an open tool call stands, as the kernel drives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Its gate answers later. The first `skip` of the decisions it gives
    /// are logged already; `resumed`, it was gated before a restart and
    /// may have run, so an unsafe one is not run again.
    Awaiting {
        skip: u16,
        resumed: bool,
    },
    /// It waits on someone: an approval, a dialog, a client, a child.
    Waiting,
    Running,
}

/// One registry generation, as the driver binds it to a session: the
/// extensions' callables, decision points and agent. Points answer from
/// what they are given and the session's committed state; each method is
/// sync and must not block (a slow answer is [`Answer::Later`]).
pub trait Registry: Send + Sync {
    /// A new agent, of the kind the session runs.
    fn agent(&self) -> Box<dyn Agent>;

    /// What the agent asks at its decisions: the handlers at the loop
    /// points. Pure, as the agent is: a load asks them again.
    fn loop_points(&self) -> &dyn LoopPoints {
        &ext::Defaults
    }

    /// The extensions it runs, in install order, as a log records them.
    fn installed(&self) -> Vec<Installed>;

    fn callable(&self, name: &str) -> Option<Callable>;

    /// The model a call goes to now, as `host:model`.
    fn model(&self) -> String {
        "none".to_owned()
    }

    /// What the model is told of where the session runs, logged once
    /// before its first call.
    fn context(&self) -> Option<String> {
        None
    }

    /// Call `tool`'s input as its tool reads it, once, and the card clients
    /// show the call with.
    fn read(&self, _tool: &str, _input: &Json) -> (Option<Parsed>, Option<Card>) {
        (None, None)
    }

    /// The `input` point: the text as the chain leaves it, or why it is
    /// refused.
    fn input(&self, text: String) -> Result<Chained<String>, String> {
        Ok(Chained::new(text))
    }

    /// The `pre_tool`, `permission` and `approve` points, merged.
    fn gate(&self, _call: CallId, _tool: &str, input: &Json) -> Answer<Gated> {
        Answer::Now(Gated::run(input.clone()))
    }

    /// The `request_error` point: how to recover model call `call` from
    /// the failure of its attempt `attempt` (from 0, counted in the log, so
    /// a restart does not start again), and who says; now or later. With
    /// no answer the call fails.
    fn request_error(
        &self,
        _call: CallId,
        _attempt: u16,
        _error: &llm::Error,
    ) -> Answer<Option<(Name, Recover)>> {
        Answer::Now(None)
    }

    /// The `post_tool` point: the outcome as the chain leaves it.
    fn post_tool(
        &self,
        _call: CallId,
        _tool: &str,
        _input: &Json,
        outcome: Outcome,
    ) -> Chained<Outcome> {
        Chained::new(outcome)
    }

    /// The `on_stop` point: text for the agent to go on with once it
    /// stopped.
    fn on_stop(&self) -> Option<(Name, String)> {
        None
    }
}

impl fmt::Debug for dyn Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Registry")
    }
}

/// The session that owns a child session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Owner {
    /// Not a child.
    #[default]
    None,
    /// A child, which reports to its owner each time it settles.
    Present,
    /// A child whose owner is gone: nothing of its turn runs again.
    Gone,
}

/// How a session's log is resumed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Load<'a> {
    /// The boot the daemon runs in: process groups logged under another
    /// one are gone, their ids reused.
    pub boot: Option<&'a str>,
    /// The first load since the daemon started: the background process
    /// groups the session owned are left over, to kill.
    pub fresh: bool,
    pub owner: Owner,
    /// Whether the session was marked busy.
    pub busy: bool,
    /// The log was just created: its first entry records the extensions
    /// the session runs with, as a later one does when they change.
    pub created: bool,
}
