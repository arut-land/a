//! Agents: what a session does with its inputs, as a fold over its log.

use protocol::{CallId, Json, Outcome, Seq, ToolChoice, ToolOutcome};
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    User {
        text: String,
    },
    ToolDone {
        call: CallId,
        outcome: ToolOutcome,
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
    /// Call `call`, detached into the background, ended with `outcome`,
    /// taken up as a turn ended: it starts one, as a child's result does.
    Detached {
        call: CallId,
        outcome: ToolOutcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
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

impl Input {
    /// The input of an agent implemented over the JSON ABI.
    pub fn to_json(&self) -> Result<Json, String> {
        Json::encode(self).map_err(|error| error.to_string())
    }
}

/// Actions returned by an agent over the JSON ABI, decoded by their owner.
pub fn actions_from_json(json: &Json) -> Result<Actions, String> {
    if json.raw() == "null" {
        return Ok(Actions::new());
    }
    json.parse::<Vec<Action>>()
        .map(Into::into)
        .map_err(|error| error.to_string())
}

/// Developer-owned admission policy for an agent kind. Names are constants
/// from extension types, for example `Except(&[Compact::NAME, Title::NAME])`.
#[derive(Clone, Copy, Debug, Default)]
pub enum Inheritance {
    #[default]
    All,
    Except(&'static [&'static str]),
    Only(&'static [&'static str]),
}

impl Inheritance {
    pub fn includes(self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Except(names) => !names.contains(&name),
            Self::Only(names) => names.contains(&name),
        }
    }
}

/// A source declaration, never part of a model's spawn arguments.
pub trait AgentKind: Send + Sync + 'static {
    const NAME: &'static str;
    const INHERIT: Inheritance = Inheritance::All;
    const INHERIT_SETTINGS: bool = true;
    /// Adds a kind's settings layer after inherited settings, before admission.
    fn settings(_states: &mut crate::States) {}
    fn make() -> Box<dyn Agent>;
}

/// One response per inbox input; provider tool requests cannot become actions.
pub struct ModelOnly;
impl Agent for ModelOnly {
    fn step(&mut self, input: Input, _: &dyn LoopPoints) -> Actions {
        match input {
            Input::User { .. } => [Action::CallModel {
                choice: ToolChoice::None,
            }]
            .into_iter()
            .collect(),
            _ => Actions::new(),
        }
    }
}
