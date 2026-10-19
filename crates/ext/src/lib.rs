//! The extension SDK: what every feature, built-in or not, is made of.
//!
//! An extension contributes through one [`Registrar`]: tools and commands,
//! handlers at the engine's decision [`point`]s, text the model reads,
//! durable [`Fold`], typed services other extensions build on, and the
//! agents a session can run. The engine itself offers the model nothing.
//!
//! Extensions talk to each other through four channels and no fifth: the
//! log (the [`point::Observe`] point), typed state, services, and points.
//!
//! An agent never performs I/O: it turns an [`Input`] into [`Action`]s, and
//! the engine commits entries, runs tools and feeds results back in. Every
//! input is a logged fact, so an agent's state is a fold over its session's
//! log.
//!
//! [`host`] is the engine's side of all this.

use std::sync::Arc;

use protocol::{Body, Outcome};
use serde_json::Value;

mod ext;
pub mod host;
pub mod point;
mod service;
mod state;
mod tool;

pub use ext::{Command, DynExtension, Extension, Facts, Place, Refused, Registrar, Text, Tier};
pub use point::{History, PointCx, Rewrite};
pub use service::{CwdOf, Spawner};
pub use state::{Fold, States};
pub use tool::{Cancel, Cx, DynTool, Kill, Replay, Route, SpawnSpec, Tool};

/// A runtime name: of an extension, a tool, an agent. Typed Rust items
/// name themselves with a `&'static str`, converted once as they register;
/// others (a Lua mod, a client's tools) are named only at run time.
pub type Name = Arc<str>;

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
        Outcome::Done { output } => match output.parse::<String>() {
            Ok(text) => format!("The child session finished:\n{text}"),
            Err(_) => format!("The child session finished:\n{output}"),
        },
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

/// A session's agent loop, as an extension registers it with
/// [`Registrar::agent`].
pub trait Agent: Send {
    fn step(&mut self, input: Input) -> Vec<Action>;
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
