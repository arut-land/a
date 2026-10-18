//! Typed tools, and the erased form a [`Toolset`] holds.

use std::path::Path;

use protocol::{Environment, Outcome};
use serde::Serialize;
use serde::de::{Deserialize, DeserializeOwned};
use serde_json::Value;

/// Whether a call interrupted by a crash may simply run again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    Safe,
    Unsafe,
}

pub struct Cx<'a> {
    /// The session's workspace.
    pub cwd: &'a Path,
    /// The attaching client's environment, which processes run in. `None`
    /// (a session no client has described) inherits the daemon's.
    pub env: Option<&'a Environment>,
    /// Tools that start processes report each process group here, so the
    /// engine can record it durably and kill it if the daemon dies mid-call.
    pub spawned: &'a dyn Fn(u32),
}

/// A tool the model can call. Its output is the stored fact; what the model
/// sees of it is [`Tool::to_model`].
pub trait Tool: Send + Sync + 'static {
    type In: DeserializeOwned;
    type Out: Serialize + DeserializeOwned;

    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    /// How a call left dangling by a crash is finished.
    const REPLAY: Replay = Replay::Unsafe;

    /// The JSON Schema of [`Tool::In`].
    fn schema() -> Value;

    fn call(&self, input: Self::In, cx: &Cx<'_>) -> Result<Self::Out, String>;

    fn to_model(output: &Self::Out) -> llm::Output {
        llm::Output::Json(serde_json::to_value(output).unwrap_or_default())
    }
}

/// A [`Tool`] over JSON, so tools of every type fit one [`Toolset`].
pub trait DynTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn replay(&self) -> Replay;
    /// How the tool is offered to the model.
    fn spec(&self) -> llm::Tool;
    fn call(&self, input: &Value, cx: &Cx<'_>) -> Outcome;
    /// What the model sees of a stored output.
    fn to_model(&self, output: &Value) -> llm::Output;
}

impl<T: Tool> DynTool for T {
    fn name(&self) -> &'static str {
        T::NAME
    }

    fn replay(&self) -> Replay {
        T::REPLAY
    }

    fn spec(&self) -> llm::Tool {
        llm::Tool {
            name: llm::SmolStr::new_static(T::NAME),
            description: T::DESCRIPTION.to_owned(),
            schema: T::schema(),
            strict: None,
        }
    }

    fn call(&self, input: &Value, cx: &Cx<'_>) -> Outcome {
        let result = T::In::deserialize(input)
            .map_err(|e| format!("invalid input: {e}"))
            .and_then(|input| Tool::call(self, input, cx))
            .and_then(|output| serde_json::to_value(output).map_err(|e| e.to_string()));
        match result {
            Ok(output) => Outcome::Done { output },
            Err(error) => Outcome::Failed { error },
        }
    }

    fn to_model(&self, output: &Value) -> llm::Output {
        T::Out::deserialize(output)
            .map_or_else(|_| llm::Output::Json(output.clone()), |o| T::to_model(&o))
    }
}

/// The tools an agent may call, by name.
pub struct Toolset(pub &'static [&'static dyn DynTool]);

impl Default for Toolset {
    /// The built-in tools.
    fn default() -> Self {
        use crate::tools::{ReadFile, RunCommand, Sleep};
        Self(&[&RunCommand, &Sleep, &ReadFile])
    }
}

impl Toolset {
    pub fn get(&self, name: &str) -> Option<&'static dyn DynTool> {
        self.0.iter().copied().find(|t| t.name() == name)
    }

    /// Every tool, as offered to the model.
    pub fn specs(&self) -> Vec<llm::Tool> {
        self.0.iter().map(|t| t.spec()).collect()
    }

    pub fn run(&self, name: &str, input: &Value, cx: &Cx<'_>) -> Outcome {
        match self.get(name) {
            Some(tool) => tool.call(input, cx),
            None => Outcome::Failed {
                error: format!("unknown tool {name}"),
            },
        }
    }

    /// What the model sees of a call's outcome.
    pub fn to_model(&self, name: &str, outcome: &Outcome) -> (llm::Output, bool) {
        match outcome {
            Outcome::Done { output } => (
                self.get(name)
                    .map_or_else(|| llm::Output::Json(output.clone()), |t| t.to_model(output)),
                false,
            ),
            Outcome::Failed { error } => (llm::Output::Text(error.clone()), true),
            Outcome::Interrupted => (
                llm::Output::Text("interrupted: the engine stopped mid-call".to_owned()),
                true,
            ),
        }
    }
}
