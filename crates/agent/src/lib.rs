//! Agents as sans-IO step machines, the tools they call, and the hooks that
//! intercept them, as extensions contribute them.
//!
//! An agent never performs I/O: it turns an [`Input`] into [`Action`]s, and the
//! engine commits entries, runs tools and feeds results back in. Every input
//! is a logged fact, so an agent's state is a fold over its session's log.

use std::str::FromStr;

use protocol::{Body, Outcome};
use serde_json::{Value, json};

pub mod builtin;
mod ext;
mod hooks;
mod state;
#[cfg(test)]
mod testing;
mod tool;
pub mod tools;

pub use builtin::{Fixtures, Policy, builtins};
pub use ext::{
    Command, Current, DynCommand, Extension, Facts, Refused, Registrar, Registry, Section, Slot,
    Tier,
};
pub use hooks::{AskFor, Decided, Gate, Hook, Hooks, Pre, Said, Spawn};
pub use state::{Cwd, State, States};
pub use tool::{Cancel, Cx, DynTool, Host, Kill, Replay, Route, Tool, Toolset, session_dir};

pub enum Input {
    User {
        text: String,
    },
    ToolDone {
        call: u64,
        outcome: Outcome,
    },
    ModelDone {
        call: u64,
        outcome: Outcome,
    },
    /// Watch `watch` fired: the child it watched settled with `outcome`.
    ChildDone {
        watch: u64,
        outcome: Outcome,
    },
}

/// The most of a child's result a parent's model sees, in bytes: about 8k
/// tokens.
pub const CHILD_RESULT_LIMIT: usize = 32 * 1024;

/// A child's result as its parent's agent reads it, cut to
/// [`CHILD_RESULT_LIMIT`].
pub fn child_result(outcome: &Outcome) -> String {
    let text = match outcome {
        Outcome::Done {
            output: Value::String(text),
        } => format!("The child session finished:\n{text}"),
        Outcome::Done { output } => format!("The child session finished:\n{output}"),
        Outcome::Failed { error } => format!("The child session failed: {error}"),
        Outcome::Interrupted { .. } => "The child session was interrupted.".to_owned(),
    };
    if text.len() > CHILD_RESULT_LIMIT {
        let cut = text.floor_char_boundary(CHILD_RESULT_LIMIT);
        let left = text.len() - cut;
        return format!("{}\n[{left} more bytes not shown]", &text[..cut]);
    }
    text
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Emit(Body),
    RunTool {
        tool: String,
        input: Value,
        /// The model's id for the call.
        id: Option<String>,
        /// Answers the call with this error instead of running it.
        refuse: Option<String>,
    },
    /// Ask the model to reply to the conversation so far.
    CallModel,
    /// Start a child session on `input` and watch it; its result comes back
    /// into this session's log when it settles.
    Spawn {
        input: String,
    },
}

pub trait Agent: Send {
    fn step(&mut self, input: Input) -> Vec<Action>;
}

/// The agents a session can run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    Echo,
    #[default]
    Chat,
}

impl Kind {
    pub fn agent(self) -> Box<dyn Agent> {
        match self {
            Self::Echo => Box::new(Echo),
            Self::Chat => Box::new(Chat::default()),
        }
    }
}

impl FromStr for Kind {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, String> {
        match name {
            "echo" => Ok(Self::Echo),
            "chat" => Ok(Self::Chat),
            _ => Err(format!("unknown agent {name}")),
        }
    }
}

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
                let Ok(reply) = llm::Response::from_value(&output) else {
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
pub struct Echo;

impl Agent for Echo {
    fn step(&mut self, input: Input) -> Vec<Action> {
        let text = match input {
            Input::User { text } => text,
            Input::ChildDone { outcome, .. } => {
                let text = child_result(&outcome);
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
            (tools::Bash::NAME, json!({ "command": command }))
        } else if let Some(Ok(seconds)) = text.strip_prefix("sleep ").map(str::parse::<u64>) {
            (tools::Sleep::NAME, json!({ "seconds": seconds }))
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
            outcome: Outcome::Done { output },
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

    #[test]
    fn a_childs_result_is_cut_for_the_model() {
        let long = "é".repeat(CHILD_RESULT_LIMIT);
        let text = child_result(&Outcome::Done {
            output: long.into(),
        });
        let (shown, note) = text.rsplit_once('\n').unwrap();
        assert!(shown.len() <= CHILD_RESULT_LIMIT);
        assert!(note.starts_with('[') && note.ends_with("more bytes not shown]"));
        let failed = Outcome::Failed {
            error: "boom".into(),
        };
        assert_eq!(child_result(&failed), "The child session failed: boom");
    }

    #[test]
    fn replay_policy_is_per_tool() {
        let registry = builtins(&Policy::default())
            .and_then(|r| r.with(&Fixtures))
            .unwrap();
        let tools = registry.tools();
        assert_eq!(tools.get("bash").unwrap().replay(), Replay::Unsafe);
        assert_eq!(tools.get("edit").unwrap().replay(), Replay::Unsafe);
        assert_eq!(tools.get("sleep").unwrap().replay(), Replay::Safe);
    }

    #[test]
    fn agents_parse_by_name() {
        assert_eq!("echo".parse(), Ok(Kind::Echo));
        assert_eq!("chat".parse(), Ok(Kind::Chat));
        assert!("other".parse::<Kind>().is_err());
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
                output: serde_json::to_value(response).unwrap(),
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
