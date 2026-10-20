//! The default loop: what a session runs when no agent is named. It hands
//! the user's message to the model, runs the tools the model calls, gives
//! the model their results, and stops once it answers with none. Each of
//! its three decisions is a loop point ([`point::Turn`], [`point::Reply`],
//! [`point::Resume`]), so a planner, a `ReAct` variant or a step limit is a
//! handler, not another loop; a wholly different loop is still an
//! [`Agent`](crate::Agent) of its own.
//!
//! The loop is a fold over the session's log, rerun as the log loads, so
//! its points are too: their handlers are given their arguments and
//! nothing of the session, so a replay asks them what the live run did.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use protocol::{Outcome, ToolChoice};
use serde_json::value::RawValue;

use crate::point::{self, Hooks};
use crate::{Action, Actions, Agent, Extension, Input, Registrar};

/// What the loop asks at its decisions. A registry's [`Hooks`] answer
/// through the handlers at the loop points; [`Defaults`] with none.
/// Every method is sync, pure and total.
pub trait LoopPoints {
    /// What an input starts: `default` is one model call.
    fn turn(&self, input: &Input, default: Actions) -> Actions;
    /// What the model's reply at step `step` becomes: `default` is a call
    /// per tool call, and a refusal for one the loop will not run.
    fn reply(&self, step: u16, reply: &llm::Response, default: Actions) -> Actions;
    /// What follows once every call of step `step`'s reply is answered.
    fn resume(&self, step: u16) -> Resume;
}

/// What follows a step whose calls are all answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Resume {
    /// Another model call.
    #[default]
    Continue,
    /// One more, the last: the tools stay offered, but the model may call
    /// none.
    Final,
    /// None: the turn ends.
    Stop,
}

/// The loop points with no handlers.
pub struct Defaults;

impl LoopPoints for Defaults {
    fn turn(&self, _: &Input, default: Actions) -> Actions {
        default
    }

    fn reply(&self, _: u16, _: &llm::Response, default: Actions) -> Actions {
        default
    }

    fn resume(&self, _: u16) -> Resume {
        Resume::Continue
    }
}

impl LoopPoints for Hooks {
    fn turn(&self, input: &Input, mut actions: Actions) -> Actions {
        let Ok(()) = self.run::<point::Turn>(&(), input, &mut actions, &mut |_, _, _| {});
        actions
    }

    fn reply(&self, step: u16, reply: &llm::Response, mut actions: Actions) -> Actions {
        let facts = (step, reply);
        let Ok(()) = self.run::<point::Reply>(&(), facts, &mut actions, &mut |_, _, _| {});
        actions
    }

    fn resume(&self, step: u16) -> Resume {
        let answer = self.run::<point::Resume>(&(), step, &mut (), &mut |_, (), ()| {});
        answer.map_or_else(Resume::default, |(_, resume)| resume)
    }
}

/// The default loop's state: a fold over its inputs.
#[derive(Debug, Default)]
pub struct Loop {
    /// Model calls since the input that started the turn.
    step: u16,
    /// Calls of the last reply still unanswered.
    pending: usize,
    /// Whether the last model call was the turn's last.
    last: bool,
}

/// The default loop's name, as `[daemon] agent` names it.
pub const LOOP: &str = "loop";

impl Agent for Loop {
    fn step(&mut self, input: Input, points: &dyn LoopPoints) -> Actions {
        match &input {
            // A child interrupted with its owner hands back nothing to go
            // on with.
            Input::ChildDone {
                outcome: Outcome::Interrupted { .. },
                ..
            } => Actions::new(),
            // A child's result arrives between turns, so it starts one.
            Input::User { .. } | Input::ChildDone { .. } => {
                *self = Self {
                    step: 1,
                    ..Self::default()
                };
                points.turn(&input, model(ToolChoice::Auto))
            }
            Input::ModelDone {
                outcome: Outcome::Done { output },
                ..
            } => {
                let Ok(reply) = llm::Response::from_json(output.raw()) else {
                    return Actions::new();
                };
                let default = self.calls(&reply);
                let actions = points.reply(self.step, &reply, default);
                self.pending = actions
                    .iter()
                    .filter(|a| matches!(a, Action::Call { .. } | Action::Refuse { .. }))
                    .count();
                actions
            }
            Input::ToolDone { .. } if self.pending > 0 => {
                self.pending -= 1;
                if self.pending > 0 || self.last {
                    return Actions::new();
                }
                let choice = match points.resume(self.step) {
                    Resume::Continue => ToolChoice::Auto,
                    Resume::Final => ToolChoice::None,
                    Resume::Stop => return Actions::new(),
                };
                self.step = self.step.saturating_add(1);
                self.last = choice == ToolChoice::None;
                model(choice)
            }
            Input::ToolDone { .. } | Input::ModelDone { .. } => Actions::new(),
        }
    }
}

impl Loop {
    /// The default answer to a reply: its calls, each run unless its
    /// arguments do not parse or the model was to call none.
    fn calls(&self, reply: &llm::Response) -> Actions {
        (reply.message.tool_calls())
            .map(|call| {
                let (tool, id) = (call.name.to_string(), Some(call.id.to_string()));
                let input = call.input.clone();
                match self.refusal(call) {
                    Some(why) => Action::Refuse {
                        tool,
                        input,
                        id,
                        why,
                    },
                    None => Action::Call { tool, input, id },
                }
            })
            .collect()
    }

    /// Why the model's call is answered without running: its arguments
    /// did not parse, or it came in the turn's last reply.
    fn refusal(&self, call: &llm::ToolCall) -> Option<String> {
        if let Some(text) = &call.invalid {
            // Why the text is no object, as reading one says.
            let why = serde_json::from_str::<BTreeMap<String, &RawValue>>(text)
                .err()
                .map_or_else(|| "not an object".to_owned(), |e| e.to_string());
            return Some(format!(
                "the arguments are not a JSON object ({why}): {text}"
            ));
        }
        self.last
            .then(|| "not run: the turn's limit of model calls is reached".to_owned())
    }
}

fn model(choice: ToolChoice) -> Actions {
    Actions::from_elem(Action::CallModel { choice }, 1)
}

/// A limit on the model calls one input leads to, as a [`point::Resume`]
/// handler: the call after step `max` is the last, offered the tools but
/// calling none. Without it, a turn runs until the model stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepLimit {
    pub max: u16,
}

impl Default for StepLimit {
    fn default() -> Self {
        Self { max: 20 }
    }
}

impl StepLimit {
    /// The answer at step `step`: none before the limit, so the next
    /// handler or the default answers.
    pub fn resume(self, step: u16) -> Option<Resume> {
        match step.cmp(&self.max) {
            Ordering::Less => None,
            Ordering::Equal => Some(Resume::Final),
            Ordering::Greater => Some(Resume::Stop),
        }
    }
}

impl Extension for StepLimit {
    const NAME: &'static str = LOOP;
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let limit = *self;
        r.on::<point::Resume>(move |(), step| limit.resume(step));
    }
}

#[cfg(test)]
mod tests;
