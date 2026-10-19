//! `chat`: the model-backed agent loop.

use ext::{Action, Agent, Extension, Input, Registrar};
use protocol::Outcome;
use serde_json::Value;

/// The agent's name.
pub const CHAT: &str = "chat";

/// The `chat` extension, which registers the [`Loop`] as [`CHAT`].
pub struct Chat;

impl Extension for Chat {
    const NAME: &'static str = "chat";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.agent(CHAT, || Box::new(Loop::default()));
    }
}

/// Model calls one user message may lead to; the calls of the last reply
/// are answered with an error instead of run.
pub const MAX_STEPS: u32 = 20;

/// Hands every user message to the model, runs the tools it calls and gives
/// it their results, until it answers without calling any.
#[derive(Default)]
pub struct Loop {
    /// Model calls since the last user message.
    steps: u32,
    /// Calls of the last reply still unanswered.
    pending: usize,
}

impl Agent for Loop {
    fn step(&mut self, input: Input) -> Vec<Action> {
        match input {
            // A child interrupted with its owner hands back nothing to go
            // on with.
            Input::ChildDone {
                outcome: Outcome::Interrupted { .. },
                ..
            } => Vec::new(),
            // A child's result arrives between turns, so it starts one.
            Input::User { .. } | Input::ChildDone { .. } => {
                (self.steps, self.pending) = (1, 0);
                vec![Action::CallModel]
            }
            Input::ModelDone {
                outcome: Outcome::Done { output },
                ..
            } => {
                let Ok(reply) = llm::Response::from_value(&output.value()) else {
                    return Vec::new();
                };
                let limited = self.steps >= MAX_STEPS;
                let actions: Vec<Action> = reply
                    .message
                    .tool_calls()
                    .map(|call| Action::RunTool {
                        tool: call.name.to_string(),
                        input: Value::Object(call.input.clone()),
                        id: Some(call.id.to_string()),
                        refuse: refusal(call, limited),
                    })
                    .collect();
                self.pending = actions.len();
                actions
            }
            Input::ToolDone { .. } if self.pending > 0 => {
                self.pending -= 1;
                if self.pending > 0 || self.steps >= MAX_STEPS {
                    return Vec::new();
                }
                self.steps += 1;
                vec![Action::CallModel]
            }
            Input::ToolDone { .. } | Input::ModelDone { .. } => Vec::new(),
        }
    }
}

/// Why a model's call is answered without running: its arguments did not
/// parse, or the step limit is reached.
fn refusal(call: &llm::ToolCall, limited: bool) -> Option<String> {
    if let Some(text) = &call.invalid {
        let why = serde_json::from_str::<serde_json::Map<String, Value>>(text)
            .err()
            .map_or_else(|| "not an object".to_owned(), |e| e.to_string());
        return Some(format!(
            "the arguments are not a JSON object ({why}): {text}"
        ));
    }
    limited.then(|| format!("not run: the limit of {MAX_STEPS} model calls per message is reached"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Input {
        Input::User { text: text.into() }
    }

    #[test]
    fn chat_hears_a_childs_result() {
        let done = |output: Value| Input::ChildDone {
            watch: 3,
            outcome: Outcome::Done {
                output: output.into(),
            },
        };
        let mut chat = Loop::default();
        assert_eq!(chat.step(done("hi".into())), [Action::CallModel]);
        assert_eq!(chat.step(reply(&[])), NOTHING);
    }

    /// A model reply holding `calls`, as the engine stores it.
    fn reply(calls: &[(&str, Option<&str>)]) -> Input {
        let content = calls
            .iter()
            .map(|(id, invalid)| {
                llm::AssistantPart::ToolCall(llm::ToolCall {
                    id: (*id).into(),
                    name: "sleep".into(),
                    input: serde_json::Map::new(),
                    invalid: invalid.map(str::to_owned),
                    provider_executed: false,
                    meta: llm::Meta::new(),
                })
            })
            .chain([llm::AssistantPart::Text(llm::Text::new("ok"))])
            .collect();
        let response = llm::Response {
            message: llm::Assistant {
                content,
                origin: None,
            },
            finish: llm::Finish::new(llm::Reason::Stop, None),
            usage: llm::Usage::default(),
            id: None,
            model: None,
            meta: llm::Meta::new(),
        };
        Input::ModelDone {
            call: 0,
            outcome: Outcome::Done {
                output: serde_json::to_value(response).unwrap().into(),
            },
        }
    }

    fn done() -> Input {
        Input::ToolDone {
            call: 0,
            outcome: Outcome::Interrupted { output: None },
        }
    }

    const NOTHING: [Action; 0] = [];

    #[test]
    fn chat_calls_the_model_again_once_every_call_is_answered() {
        let mut chat = Loop::default();
        assert_eq!(chat.step(user("hi")), [Action::CallModel]);
        let calls = chat.step(reply(&[("a", None), ("b", Some("{\"sec"))]));
        let ids: Vec<_> = calls
            .iter()
            .map(|action| match action {
                Action::RunTool { id, refuse, .. } => (id.as_deref(), refuse.is_some()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ids, [(Some("a"), false), (Some("b"), true)]);
        assert_eq!(chat.step(done()), NOTHING);
        assert_eq!(chat.step(done()), [Action::CallModel]);
        assert_eq!(chat.step(reply(&[])), NOTHING);
        // A stray result, as an odd log might hold, changes nothing.
        assert_eq!(chat.step(done()), NOTHING);
    }

    #[test]
    fn chat_stops_at_the_step_limit() {
        let mut chat = Loop::default();
        chat.step(user("hi"));
        for _ in 1..MAX_STEPS {
            assert_eq!(chat.step(reply(&[("a", None)])).len(), 1);
            assert_eq!(chat.step(done()), [Action::CallModel]);
        }
        let last = chat.step(reply(&[("a", None)]));
        assert!(
            matches!(&last[..], [Action::RunTool { refuse: Some(why), .. }] if why.contains("limit"))
        );
        assert_eq!(chat.step(done()), NOTHING);
    }
}
