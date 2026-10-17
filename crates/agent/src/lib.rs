//! Agents as sans-IO step machines, and the tools they call.
//!
//! An agent never performs I/O: it turns an [`Input`] into [`Action`]s, and the
//! engine commits entries, runs tools and feeds results back in.

use std::path::Path;

use protocol::{Body, Outcome};
use serde_json::{Value, json};

pub mod tools;

pub enum Input {
    User { text: String },
    ToolDone { call: u64, outcome: Outcome },
}

pub enum Action {
    Emit(Body),
    RunTool { tool: String, input: Value },
}

pub trait Agent: Send {
    fn step(&mut self, input: Input) -> Vec<Action>;
}

/// Echoes messages back. `$ <command>` runs a shell command and
/// `sleep <seconds>` sleeps, so tool calls can be exercised without a model.
pub struct Echo;

impl Agent for Echo {
    fn step(&mut self, input: Input) -> Vec<Action> {
        let Input::User { text } = input else {
            return Vec::new();
        };
        if let Some(command) = text.strip_prefix("$ ") {
            return vec![Action::RunTool {
                tool: "run_command".into(),
                input: json!({ "command": command }),
            }];
        }
        if let Some(Ok(seconds)) = text.strip_prefix("sleep ").map(str::parse::<u64>) {
            return vec![Action::RunTool {
                tool: "sleep".into(),
                input: json!({ "seconds": seconds }),
            }];
        }
        vec![Action::Emit(Body::Assistant { text })]
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

pub struct Toolset {
    tools: Vec<Box<dyn Tool>>,
}

impl Toolset {
    pub fn builtin() -> Self {
        Self {
            tools: vec![Box::new(tools::RunCommand), Box::new(tools::Sleep)],
        }
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(AsRef::as_ref)
    }

    pub fn run(&self, name: &str, input: &Value, cx: &Cx<'_>) -> Outcome {
        match self.get(name) {
            None => Outcome::Failed {
                error: format!("unknown tool {name}"),
            },
            Some(tool) => match tool.call(input, cx) {
                Ok(output) => Outcome::Done { output },
                Err(error) => Outcome::Failed { error },
            },
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
        let tools = Toolset::builtin();
        assert_eq!(tools.get("run_command").unwrap().replay(), Replay::Unsafe);
        assert_eq!(tools.get("sleep").unwrap().replay(), Replay::Safe);
    }
}
