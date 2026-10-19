//! The agent loops the built-ins register.

#[cfg(any(test, feature = "fixtures"))]
use ext::Tool;
use ext::{Action, Agent, Input};
#[cfg(any(test, feature = "fixtures"))]
use protocol::Body;
use protocol::Outcome;
use serde_json::Value;
#[cfg(any(test, feature = "fixtures"))]
use serde_json::json;

/// The model-backed agent's name, which sessions run unless configured
/// otherwise.
pub const CHAT: &str = "chat";
/// The test agent's name.
pub const ECHO: &str = "echo";

/// Model calls one user message may lead to; the calls of the last reply
/// are answered with an error instead of run.
pub const MAX_STEPS: u32 = 20;

/// Hands every user message to the model, runs the tools it calls and gives
/// it their results, until it answers without calling any.
#[derive(Default)]
pub struct Chat {
    /// Model calls since the last user message.
    steps: u32,
    /// Calls of the last reply still unanswered.
    pending: usize,
}

impl Agent for Chat {
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

/// Echoes messages back. `$ <command>` runs it with `bash`,
/// `sleep <seconds>` calls the `sleep` fixture and `spawn <message>` hands
/// the message to a child session, so tool calls and links can be exercised
/// without a model.
#[cfg(any(test, feature = "fixtures"))]
pub struct Echo;

#[cfg(any(test, feature = "fixtures"))]
impl Agent for Echo {
    fn step(&mut self, input: Input) -> Vec<Action> {
        let text = match input {
            Input::User { text } => text,
            Input::ChildDone { outcome, .. } => {
                let text = ext::child_result(&outcome);
                return vec![Action::Emit(Body::Assistant { text })];
            }
            Input::ToolDone { .. } | Input::ModelDone { .. } => return Vec::new(),
        };
        if let Some(input) = text.strip_prefix("spawn ") {
            return vec![Action::Spawn {
                input: input.to_owned(),
            }];
        }
        let (tool, input) = if let Some(command) = text.strip_prefix("$ ") {
            (crate::tools::Bash::NAME, json!({ "command": command }))
        } else if let Some(Ok(seconds)) = text.strip_prefix("sleep ").map(str::parse::<u64>) {
            (crate::tools::Sleep::NAME, json!({ "seconds": seconds }))
        } else {
            return vec![Action::Emit(Body::Assistant { text })];
        };
        vec![Action::RunTool {
            tool: tool.to_owned(),
            input,
            id: None,
            refuse: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Input {
        Input::User { text: text.into() }
    }

    fn tool_named(actions: &[Action]) -> Option<&str> {
        match actions {
            [Action::RunTool { tool, .. }] => Some(tool),
            _ => None,
        }
    }

    #[test]
    fn echo_routes_messages_and_tools() {
        let mut echo = Echo;
        assert!(
            matches!(&echo.step(user("hi"))[..], [Action::Emit(Body::Assistant { text })] if text == "hi")
        );
        assert_eq!(tool_named(&echo.step(user("$ ls"))), Some("bash"));
        assert_eq!(tool_named(&echo.step(user("sleep 2"))), Some("sleep"));
        assert!(matches!(&echo.step(user("sleep x"))[..], [Action::Emit(_)]));
        assert_eq!(
            echo.step(user("spawn sleep 1")),
            [Action::Spawn {
                input: "sleep 1".into()
            }]
        );
    }

    #[test]
    fn agents_hear_a_childs_result() {
        let done = |output: Value| Input::ChildDone {
            watch: 3,
            outcome: Outcome::Done {
                output: output.into(),
            },
        };
        assert_eq!(
            Echo.step(done("hi".into())),
            [Action::Emit(Body::Assistant {
                text: "The child session finished:\nhi".into()
            })]
        );
        let mut chat = Chat::default();
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
        let mut chat = Chat::default();
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
        let mut chat = Chat::default();
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
