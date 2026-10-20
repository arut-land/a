//! Agents: what a session does with its inputs, as a fold over its log.

use protocol::{CallId, Json, Outcome, Seq, ToolChoice};
use smallvec::SmallVec;

use crate::LoopPoints;

/// What an agent asks for, in order.
pub type Actions = SmallVec<[Action; 2]>;

/// A session's agent: turns each input into what it asks for next, and
/// performs no I/O. Its inputs are logged facts, so it is a fold over its
/// session's log. The default is [`Loop`](crate::Loop), which asks
/// `points` at each of its decisions; an extension registers another with
/// [`Registrar::agent`](crate::Registrar::agent). Each turn is stepped by
/// one made for it, from the input that starts it (a user's, a child's
/// result), so nothing an agent holds outlives its turn.
pub trait Agent: Send {
    fn step(&mut self, input: Input, points: &dyn LoopPoints) -> Actions;
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
    /// Ask the model to reply to the conversation so far, with the tools
    /// `choice` lets it call.
    CallModel { choice: ToolChoice },
}
