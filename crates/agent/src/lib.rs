//! Agents as sans-IO step machines, and the tools they call.
//!
//! An agent never performs I/O: it turns an [`Input`] into [`Action`]s, and the
//! engine commits entries, runs tools and feeds results back in.

use std::path::Path;
use std::str::FromStr;

use protocol::{Body, Outcome};
use serde_json::{Value, json};

pub mod tools;

pub enum Input {
    User { text: String },
    ToolDone { call: u64, outcome: Outcome },
    ModelDone { call: u64, outcome: Outcome },
}

pub enum Action {
    Emit(Body),
    RunTool {
        tool: String,
        input: Value,
    },
    /// Ask the model to reply to the conversation so far.
    CallModel,
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
            Self::Chat => Box::new(Chat),
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

/// Hands every user message to the model.
pub struct Chat;

impl Agent for Chat {
    fn step(&mut self, input: Input) -> Vec<Action> {
        match input {
            Input::User { .. } => vec![Action::CallModel],
            Input::ToolDone { .. } | Input::ModelDone { .. } => Vec::new(),
        }
    }
}

/// Echoes messages back. `$ <command>` runs a shell command and
/// `sleep <seconds>` sleeps, so tool calls can be exercised without a model.
pub struct Echo;

impl Agent for Echo {
    fn step(&mut self, input: Input) -> Vec<Action> {
        let Input::User { text } = input else {
            return Vec::new();
        };
        let (tool, input) = if let Some(command) = text.strip_prefix("$ ") {
            (tools::RunCommand.name(), json!({ "command": command }))
        } else if let Some(Ok(seconds)) = text.strip_prefix("sleep ").map(str::parse::<u64>) {
            (tools::Sleep.name(), json!({ "seconds": seconds }))
        } else {
            return vec![Action::Emit(Body::Assistant { text })];
        };
        vec![Action::RunTool {
            tool: tool.to_owned(),
            input,
        }]
    }
}

/// Whether a call interrupted by a crash may simply run again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    Safe,
    Unsafe,
}

pub struct Cx<'a> {
    pub cwd: &'a Path,
    /// Tools that start processes report each process group here, so the
    /// engine can record it durably and kill it if the daemon dies mid-call.
    pub spawned: &'a dyn Fn(u32),
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;

    fn replay(&self) -> Replay {
        Replay::Unsafe
    }

    fn call(&self, input: &Value, cx: &Cx<'_>) -> Result<Value, String>;
}

/// The tools an agent may call, by name.
pub struct Toolset(&'static [&'static dyn Tool]);

impl Default for Toolset {
    /// The built-in tools.
    fn default() -> Self {
        Self(&[&tools::RunCommand, &tools::Sleep])
    }
}

impl Toolset {
    pub fn get(&self, name: &str) -> Option<&'static dyn Tool> {
        self.0.iter().copied().find(|t| t.name() == name)
    }

    pub fn run(&self, name: &str, input: &Value, cx: &Cx<'_>) -> Outcome {
        let Some(tool) = self.get(name) else {
            return Outcome::Failed {
                error: format!("unknown tool {name}"),
            };
        };
        match tool.call(input, cx) {
            Ok(output) => Outcome::Done { output },
            Err(error) => Outcome::Failed { error },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Input {
        Input::User { text: text.into() }
    }

    #[test]
    fn echo_routes_messages_and_tools() {
        let mut echo = Echo;
        assert!(
            matches!(&echo.step(user("hi"))[..], [Action::Emit(Body::Assistant { text })] if text == "hi")
        );
        assert!(
            matches!(&echo.step(user("$ ls"))[..], [Action::RunTool { tool, .. }] if tool == "run_command")
        );
        assert!(
            matches!(&echo.step(user("sleep 2"))[..], [Action::RunTool { tool, .. }] if tool == "sleep")
        );
        assert!(matches!(&echo.step(user("sleep x"))[..], [Action::Emit(_)]));
    }

    #[test]
    fn replay_policy_is_per_tool() {
        let tools = Toolset::default();
        assert_eq!(tools.get("run_command").unwrap().replay(), Replay::Unsafe);
        assert_eq!(tools.get("sleep").unwrap().replay(), Replay::Safe);
    }

    #[test]
    fn agents_parse_by_name() {
        assert_eq!("echo".parse(), Ok(Kind::Echo));
        assert_eq!("chat".parse(), Ok(Kind::Chat));
        assert!("other".parse::<Kind>().is_err());
    }
}
