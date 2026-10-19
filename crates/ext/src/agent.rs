//! Agents: what a session does with its inputs, as a fold over its log.

use protocol::{CallId, Json, Outcome, Seq};
use smallvec::SmallVec;

/// What an agent asks for, in order.
pub type Actions = SmallVec<[Action; 2]>;

/// A session's agent: turns each input into what it asks for next, and
/// performs no I/O. Its inputs are logged facts, so it is a fold over its
/// session's log. An extension registers one with
/// [`Registrar::agent`](crate::Registrar::agent).
pub trait Agent: Send {
    fn step(&mut self, input: Input) -> Actions;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    User {
        text: String,
    },
    ToolDone {
        call: CallId,
        outcome: Outcome,
    },
    ModelDone {
        call: CallId,
        outcome: Outcome,
    },
    /// Watch `watch` fired: the session it watched settled with `outcome`.
    /// Only a watch on a session the agent did not link comes here: a
    /// linked child's result is its tool call's.
    ChildDone {
        watch: Seq,
        outcome: Outcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// An answer of its own, not a model's.
    Say(String),
    /// Call `tool`; `id` is the model's id for the call.
    Call {
        tool: String,
        input: Json,
        id: Option<String>,
    },
    /// Answer a call the model made with `why`, without running it.
    Refuse {
        tool: String,
        input: Json,
        id: Option<String>,
        why: String,
    },
    /// Ask the model to reply to the conversation so far.
    CallModel,
}

/// The agent of a registry with none.
pub(crate) struct Idle;

impl Agent for Idle {
    fn step(&mut self, _: Input) -> Actions {
        Actions::new()
    }
}
