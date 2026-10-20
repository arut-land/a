//! Services: typed capabilities one extension provides and others build
//! on, by type rather than by knowing each other. Each has at most one
//! provider per registry, and an extension that needs one no extension
//! provides is refused as it installs. The engine provides the ones only
//! it can, such as [`Settings`], to every call.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::BoxFuture;
use protocol::{Environment, Outcome, Overrides};

use crate::{Cx, Name, SpawnSpec, States};

/// Where a session's commands run. The shell provides it; what works
/// where they do (`edit`, the context the model is first given) asks it.
pub trait CwdOf: Send + Sync {
    /// The directory, for a session in `workspace` whose client's
    /// environment is `env`, with its extensions' `states`.
    fn dir(&self, workspace: &Path, env: Option<&Environment>, states: &States) -> PathBuf;
}

/// Starts the processes tools run: one extension owns how (the shell).
pub trait Spawner: Send + Sync {
    /// The process `spawn` describes, as the `spawn` point leaves it,
    /// ready to start.
    fn process(&self, cx: &Cx, spawn: SpawnSpec) -> Result<Command, String>;
}

/// A session's settings over the layers the engine reads them from: the
/// session's own, then its workspace's and the home config file. The
/// engine provides it.
pub trait Settings: Send + Sync {
    /// The model the call's session talks to, as `host:model`, and which
    /// layer names it.
    fn model(&self, cx: &Cx) -> Result<(String, String), String>;
    /// Makes `set` the session's own settings, over every layer; logged
    /// once it resolves.
    fn set<'a>(&'a self, cx: &'a Cx, set: Overrides) -> BoxFuture<'a, io::Result<()>>;
}

/// Model requests on behalf of a call, beside the session's own: what
/// compacts a transcript, judges a call or titles a session asks. The
/// engine provides it.
pub trait Models: Send + Sync {
    /// The reply to `side`, asked on behalf of the call `cx` runs: logged
    /// under that call as a model call of its own, with its purpose,
    /// counted in usage, held to its budget, and dropped with the call.
    /// Sent once: a failure is the handler's to decide on.
    fn call<'a>(
        &'a self,
        cx: &'a Cx,
        side: Side,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>>;
}

/// A side model request.
#[derive(Clone, Debug)]
pub struct Side {
    /// What it is for, as logged: `compact`, `judge`, `title`.
    pub purpose: Name,
    /// The model, as `host:model`; `None` for the session's.
    pub model: Option<String>,
    /// The request; its model is set from `model`.
    pub request: llm::Request,
    /// Whether the request's messages follow the conversation its call's
    /// model sees (system prompt included, so a provider's cache of it
    /// serves): what summarises or titles the conversation.
    pub conversation: bool,
    pub budget: Budget,
}

impl Side {
    /// A request for `purpose` of the session's model, of `messages`.
    pub fn new(purpose: &str, messages: llm::Messages) -> Self {
        Self {
            purpose: Name::from(purpose),
            model: None,
            request: llm::Request {
                messages,
                ..llm::Request::default()
            },
            conversation: false,
            budget: Budget::default(),
        }
    }

    /// The same, following the conversation its call's model sees.
    #[must_use]
    pub fn after_the_conversation(self) -> Self {
        Self {
            conversation: true,
            ..self
        }
    }
}

/// What a side request may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// The most output tokens it may ask for.
    pub max_tokens: u32,
    /// How long it may take; it fails after.
    pub timeout: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_tokens: 4096,
            timeout: Duration::from_secs(60),
        }
    }
}

/// Child sessions: a call starts one it owns and waits for what it settles
/// with. The engine provides it.
pub trait Sessions: Send + Sync {
    /// What a child session started on `input` settles with. The child is
    /// the call's own: started once however often the call runs again (as
    /// recovery runs it), and heard from once. An interrupt of the call's
    /// turn reaches the child first.
    fn child<'a>(&'a self, cx: &'a Cx, input: String) -> BoxFuture<'a, Outcome>;
}
