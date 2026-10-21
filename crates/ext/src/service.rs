//! Services: typed capabilities one extension provides and others build
//! on, by type rather than by knowing each other. Each has at most one
//! provider per registry, and an extension that needs one no extension
//! provides is refused as it installs (or, in a static assembly, does not
//! compile). The engine provides the ones only it can, such as
//! [`Layers`], to every call.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::BoxFuture;
use protocol::{Environment, Outcome};

use crate::{Cx, SpawnSpec, States};

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

/// The config layers the engine reads: a workspace's file, then the home
/// file, then the built-in default. The engine provides it.
pub trait Layers: Send + Sync {
    /// The model a session in `workspace` talks to as the layers name it,
    /// as `host:model`, and which layer names it; or why there is none.
    fn model(&self, workspace: &Path) -> Result<(String, String), String>;
}

/// Which model a session's calls go to: the session's own choice, kept as
/// an extension's state, over the [`Layers`]. An extension provides it
/// (`settings`); with none, the engine reads the layers alone.
pub trait ModelSpec: Send + Sync {
    /// The model, as `host:model`, and what names it.
    fn model(
        &self,
        states: &States,
        layers: &dyn Layers,
        workspace: &Path,
    ) -> Result<(String, String), String>;
}

/// Model requests owned by a session, outside its main transcript.
pub trait Models: Send + Sync {
    /// Reuse the committed fact for this extension's key, or admit it and
    /// return only after its result commits. A lost attempt is interrupted,
    /// never repeated by recovery. The engine provides this service.
    fn call<'a>(
        &'a self,
        cx: &'a Cx,
        side: Side,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>>;
}

/// Routing intent, never a provider model id.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum ModelRole {
    Session,
    Fast,
}

/// The immutable transcript prefix visible to a side request.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum SideContext {
    None,
    /// The effective conversation strictly before the owning call.
    BeforeOwner,
}

/// A side request whose result the caller needs before continuing.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Side {
    pub purpose: String,
    owner: String,
    pub role: ModelRole,
    pub messages: llm::Messages,
    pub context: SideContext,
    pub budget: Budget,
    background: bool,
    session_key: bool,
    while_waiting: bool,
}

impl Side {
    /// Match the extension namespace and key policy, independently of
    /// mutable config or a newly assembled prompt during recovery.
    pub fn matches_intent(&self, intent: &llm::Json) -> bool {
        #[derive(serde::Deserialize)]
        struct Identity {
            owner: String,
            purpose: String,
            session_key: bool,
        }
        #[derive(serde::Deserialize)]
        struct Intent {
            request: Identity,
        }
        intent.parse::<Intent>().is_ok_and(|i| {
            i.request.owner == self.owner
                && i.request.purpose == self.purpose
                && i.request.session_key == self.session_key
        })
    }

    /// Reuse this purpose across owners, for session metadata such as a title.
    #[must_use]
    pub fn once_per_session(mut self) -> Self {
        self.session_key = true;
        self
    }

    pub const fn has_session_key(&self) -> bool {
        self.session_key
    }

    /// The prompt and its selected transcript stay separate in the intent.
    pub fn intent(&self, transcript: &llm::Messages) -> Result<llm::Json, String> {
        #[derive(serde::Serialize)]
        struct Intent<'a> {
            request: &'a Side,
            transcript: &'a llm::Messages,
        }
        llm::Json::encode(&Intent {
            request: self,
            transcript,
        })
        .map_err(|e| e.to_string())
    }

    pub const fn is_while_waiting(&self) -> bool {
        self.while_waiting
    }

    pub const fn is_background(&self) -> bool {
        self.background
    }

    pub fn new<E: crate::Extension>(
        purpose: &str,
        role: ModelRole,
        context: SideContext,
        messages: llm::Messages,
    ) -> Self {
        Self {
            purpose: purpose.to_owned(),
            owner: E::NAME.to_owned(),
            role,
            messages,
            context,
            budget: Budget::default(),
            background: false,
            session_key: false,
            while_waiting: false,
        }
    }

    /// A background request cannot be awaited for a model result.
    pub fn background(mut self) -> Background {
        self.background = true;
        Background(self)
    }
}

/// Observation work: completion is a logged fact, never a main-turn reply.
///
/// ```compile_fail
/// # fn awaited(models: &dyn ext::Models, cx: &ext::Cx, background: ext::Background) {
/// let result = models.call(cx, background);
/// # }
/// ```
pub struct Background(Side);

impl Background {
    /// Scope optional work to the owner's current question. Answering the
    /// question cancels it even when the approved owner keeps running.
    #[must_use]
    pub fn while_waiting(mut self) -> Self {
        self.0.while_waiting = true;
        self
    }
    pub fn start(self, cx: Cx) -> crate::Reaction {
        crate::Reaction::later(crate::Requirement::Ordered, async move {
            if let Some(models) = cx.models() {
                // The service owns and commits both success and failure.
                let _ = models.call(&cx, self.0).await;
            }
        })
    }
}

/// Retry policy shared by main and side requests. The provider of this
/// service owns the policy; the engine owns cancellation and persistence.
pub trait SideRetry: Send + Sync {
    fn name(&self) -> &'static str;
    fn after(&self, error: &llm::Error, attempt: u16, seed: u64) -> Option<Duration>;
}

/// What a side request may take, including retries and their backoffs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Budget {
    pub max_tokens: std::num::NonZeroU32,
    pub timeout: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_tokens: std::num::NonZeroU32::new(4096).unwrap(),
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

/// An extension-supplied model transport for side requests, useful for
/// deterministic fixtures. It still runs behind the engine's log barriers.
pub trait SideProvider: Send + Sync {
    fn reply<'a>(
        &'a self,
        side: &'a Side,
        messages: &'a llm::Messages,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>>;
}
