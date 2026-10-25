//! The extension SDK, which every `ext-*` crate builds on: what every
//! feature, compiled in or loaded, is made of.
//!
//! An extension contributes through one [`Registrar`]: tools and commands,
//! [`query`]s clients ask, handlers at the engine's decision [`point`]s,
//! text the model reads, durable [`Fold`], typed services other extensions
//! build on, and the agents a session can run. The engine itself offers
//! the model nothing.
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

use protocol::{Outcome, Payload, ToolOutcome};

mod agent;
mod assemble;
pub use blob;
mod conversation;
mod ext;
mod flag;
mod history;
pub mod host;
mod r#loop;
pub mod point;
pub mod query;
mod queue;
pub mod schema;
mod service;
pub use conversation::{
    Background, Context, ContextPolicy, Conversation, ConversationHandle, Conversations,
    DepthBudget, Ephemeral, ModelTurn, OwnerScope, Persisted, Placement, Recipient, Runner,
    SendMessage, SideIdentity, Spawn, SpawnRefusal, Storage, Task, ThreadKey, Turn, TurnKey,
};
mod settings;
pub mod shape;
mod shared;
pub mod source;
mod state;
#[cfg(feature = "testing")]
pub mod testing;
mod tool;

pub use agent::{
    Action, Actions, Agent, AgentKind, Inheritance, Input, ModelOnly, actions_from_json,
};
pub use assemble::{Need, Provides};
pub use ext::{DynExtension, Extension, Facts, Place, Refused, Registrar, Text, Tier};
pub use flag::{Cancel, Flag};
pub use llm::Json;
pub use r#loop::{Defaults, LOOP, Loop, LoopPoints, Resume, StepLimit};
pub use point::{
    Answer, Asked, Chained, Gate, Gated, Gating, Later, Placed, PointCx, Presented, Reaction,
    Recover, Requirement, Rewrite, Stored,
};
pub use protocol::{Annotation, AnnotationLifetime, ExtensionFact};
pub use query::{Query, QueryCx};
pub use queue::{Boundary, QueueView, Take};
pub use schema::Schema;
pub use service::{
    Budget, CwdOf, Holds, LaunchSpec, Layers, ModelRole, ModelRoles, ModelSpec, Native,
    OwnedProcess, Parting, ProcessLease, ProcessOutput, ProcessPipe, ProcessSignal, Processes,
    Recipients, Record, Role, Route, RunningProcess, SideProvider, SideRetry, SpawnError, Topology,
};
pub use settings::{Configured, Empty, Table};
pub use shape::{Output, Partials, Streamed, stream};
pub use shared::WorkspaceShared;
pub use state::{Checkpoint, DynFold, Fold, Image, PreparedPublication, Publication, States};
pub use tool::{
    Args, Callable, Called, Concurrency, Cx, Detach, DynTool, MutationError, MutationResult,
    Parsed, Replay, Resumable, SpawnSpec, Tool,
};

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
/// [`CHILD_RESULT_LIMIT`]: of one a blob holds, `shown`, what its blob
/// shows the model as read back (as [`Registry::present_result`] shows a
/// tool's), when it can be read.
///
/// [`Registry::present_result`]: crate::host::Registry::present_result
pub fn child_result(outcome: &ToolOutcome, shown: Option<&str>) -> String {
    let text = match outcome {
        Outcome::Done {
            output: Payload::Inline(output),
        } => match output.parse::<String>() {
            Ok(text) => format!("The child session finished:\n{text}"),
            Err(_) => format!("The child session finished:\n{output}"),
        },
        Outcome::Done {
            output: Payload::Blob(blob),
        } => match shown {
            Some(shown) => format!("The child session finished:\n{shown}"),
            None => format!(
                "The child session finished: its {} bytes of result are held as blob {}, \
                 unreadable now.",
                blob.size, blob.id
            ),
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

/// Committed suffixes selected by immutable ownership, without writer leases.
pub trait Logs: Send + Sync {
    fn read(
        &self,
        root: protocol::SessionId,
        descendants: bool,
        cursors: Vec<(protocol::SessionId, LogCursor)>,
    ) -> BoxFuture<'_, Result<Vec<LogPrefix>, String>>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogCursor {
    pub seq: u64,
    pub epoch: protocol::Epoch,
    pub offset: u64,
}

pub struct LogPrefix {
    pub session: protocol::SessionId,
    pub entries: Vec<protocol::Entry>,
    pub cursor: LogCursor,
    pub reset: bool,
    pub inherited_through: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_childs_result_is_cut_for_the_model() {
        let long = "é".repeat(CHILD_RESULT_LIMIT);
        let text = child_result(
            &Outcome::Done {
                output: Json::encode(&long).unwrap().into(),
            },
            None,
        );
        let (shown, note) = text.rsplit_once('\n').unwrap();
        assert!(shown.len() <= CHILD_RESULT_LIMIT);
        assert!(note.starts_with('[') && note.ends_with("more bytes not shown]"));
        let failed = Outcome::Failed {
            error: "boom".into(),
        };
        assert_eq!(
            child_result(&failed, None),
            "The child session failed: boom"
        );
    }
}
