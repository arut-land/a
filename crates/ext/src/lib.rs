//! The extension SDK, which every `ext-*` crate builds on: what every
//! feature, compiled in or loaded, is made of.
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

// A tool's call shape is the type its `call` returns, named in bounds.
#![feature(return_type_notation)]
// An extension needs nothing unless it says so.
#![feature(associated_type_defaults)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use protocol::Outcome;

mod agent;
mod assemble;
mod conversation;
mod ext;
mod flag;
mod history;
pub mod host;
mod r#loop;
pub mod point;
pub mod schema;
mod service;
pub use conversation::{
    Awaited, ContextSource, ConversationHandle, DepthBudget, Detached, Runner, SendMessage, Spawn,
    SpawnRefusal,
};
mod settings;
pub mod shape;
mod state;
#[cfg(feature = "testing")]
pub mod testing;
mod tool;

pub use agent::{Action, Actions, Agent, AgentKind, Inheritance, Input, actions_from_json};
pub use assemble::{Need, Provides};
pub use ext::{DynExtension, Extension, Facts, Place, Refused, Registrar, Text, Tier};
pub use flag::{Cancel, Flag};
pub use llm::Json;
pub use r#loop::{Defaults, LOOP, Loop, LoopPoints, Resume, StepLimit};
pub use point::{
    Answer, Asked, Chained, Gate, Gated, Gating, Later, PointCx, Reaction, Recover, Requirement,
    Rewrite,
};
pub use schema::Schema;
pub use service::{
    Background, Budget, CwdOf, InboxAccess, Layers, ModelRole, ModelRoles, ModelSpec, Models,
    Sessions, Side, SideContext, SideProvider, SideRetry, Spawner,
};
pub use settings::{Configured, Empty, Table};
pub use shape::{Output, Partials, Streamed, stream};
pub use state::{Checkpoint, DynFold, Fold, Image, Publication, States};
pub use tool::{Args, Callable, Called, Concurrency, Cx, DynTool, Parsed, Replay, SpawnSpec, Tool};

/// A runtime name: of an extension, a tool, an agent. Typed Rust items
/// name themselves with a `&'static str`, converted once as they register;
/// others (a Lua mod, a client's tools) are named only at run time.
pub type Name = Arc<str>;

/// A future that can be sent across threads: what the engine runs for an
/// extension, on whichever runtime it has.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_childs_result_is_cut_for_the_model() {
        let long = "é".repeat(CHILD_RESULT_LIMIT);
        let text = child_result(&Outcome::Done {
            output: Json::encode(&long).unwrap(),
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
