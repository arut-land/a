//! Services: typed capabilities one extension provides and others build
//! on, by type rather than by knowing each other. Each has at most one
//! provider per registry, and an extension that needs one no extension
//! provides is refused as it installs (or, in a static assembly, does not
//! compile). The engine provides the ones only it can, such as
//! [`Layers`], to every call.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::BoxFuture;
use protocol::Environment;

use crate::{Called, Cx, DynTool, SpawnSpec, States};

/// Where a session's commands run. The shell provides it; what works
/// where they do (`edit`, the context the model is first given) asks it.
pub trait CwdOf: Send + Sync {
    /// The directory, for a session in `workspace` whose client's
    /// environment is `env`, with its extensions' `states`.
    fn dir(&self, workspace: &Path, env: Option<&Environment>, states: &States) -> PathBuf;
}

/// What a call's result is recorded as, asked once as the call ends and
/// before the engine logs it, with the call's context: the one moment a
/// result may be changed with I/O (output sealed into a blob the result
/// then holds), so what is logged is the fact, and every projection of it
/// stays pure. A call run again is recorded again.
pub trait Record: Send + Sync {
    fn record<'a>(
        &'a self,
        tool: &'a dyn DynTool,
        called: Called,
        cx: &'a Cx,
    ) -> BoxFuture<'a, Called>;
}

/// Launch and live ownership are one operation; numeric ids are recovery facts.
pub trait Processes: Send + Sync {
    fn spawn<'a>(
        &'a self,
        cx: &'a Cx,
        launch: LaunchSpec,
    ) -> BoxFuture<'a, Result<OwnedProcess, SpawnError>>;
}

pub type SpawnError = std::io::Error;

pub struct LaunchSpec {
    pub process: SpawnSpec,
    pub output: ProcessOutput,
}

pub enum ProcessOutput {
    File(std::fs::File),
    Stdio,
}

#[derive(Clone, Copy)]
pub enum ProcessSignal {
    Terminate,
    Kill,
}

/// The provider owns registration and native resources behind this port.
pub trait RunningProcess: Send + Sync {
    fn id(&self) -> u32;
    fn signal(&self, signal: ProcessSignal) -> std::io::Result<()>;
    fn alive(&self) -> bool;
    fn try_wait(&self) -> std::io::Result<Option<std::process::ExitStatus>>;
    fn wait(&self) -> BoxFuture<'_, std::io::Result<std::process::ExitStatus>>;
    /// Captured stdout has reached its output file, independently of process reaping.
    fn output_drained(&self) -> BoxFuture<'_, std::io::Result<()>>;
    fn ended(&self) -> BoxFuture<'_, ()>;
}

pub trait ProcessPipe: Send + Sync {
    fn read<'a>(&'a self, bytes: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>>;
    fn write<'a>(&'a self, bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<usize>>;
    fn try_write(&self, bytes: &[u8]) -> std::io::Result<usize>;
    /// Completes accepted writes before the caller reports a flushed request.
    fn flush(&self) -> BoxFuture<'_, std::io::Result<()>>;
}

pub struct OwnedProcess {
    pub process: ProcessLease,
    pub input: Option<Box<dyn ProcessPipe>>,
    pub output: Option<Box<dyn ProcessPipe>>,
}

impl OwnedProcess {
    pub fn new(
        process: Arc<dyn RunningProcess>,
        input: Option<Box<dyn ProcessPipe>>,
        output: Option<Box<dyn ProcessPipe>>,
    ) -> Self {
        Self {
            process: process.into(),
            input,
            output,
        }
    }

    /// Transfer cancellation to the session while retaining native ownership.
    pub fn detach(self, cx: &Cx) {
        cx.own(self.process);
    }
}

impl std::ops::Deref for OwnedProcess {
    type Target = dyn RunningProcess;
    fn deref(&self) -> &Self::Target {
        &**self.process
    }
}

/// Session cancellation owns a control lease independently of pipe ownership.
pub struct ProcessLease {
    process: Arc<dyn RunningProcess>,
    released: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl ProcessLease {
    /// Completion and rejected adoption must both release dependent resources.
    #[must_use]
    pub fn on_release(mut self, released: impl FnOnce() + Send + Sync + 'static) -> Self {
        self.released = Some(Box::new(released));
        self
    }
}

impl From<Arc<dyn RunningProcess>> for ProcessLease {
    fn from(process: Arc<dyn RunningProcess>) -> Self {
        Self {
            process,
            released: None,
        }
    }
}

impl std::ops::Deref for ProcessLease {
    type Target = Arc<dyn RunningProcess>;
    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

impl Drop for ProcessLease {
    fn drop(&mut self) {
        let _ = self.signal(ProcessSignal::Kill);
        if let Some(released) = self.released.take() {
            released();
        }
    }
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

/// Routing intent, never a provider model id.
#[derive(Clone, Debug)]
pub enum ModelRole {
    Session,
    Fast,
    Named(String),
}

pub trait ModelRoles: Send + Sync {
    fn model(
        &self,
        role: &str,
        states: &States,
        layers: &dyn Layers,
        workspace: &Path,
    ) -> Result<String, String>;
}

impl std::fmt::Display for ModelRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session => f.write_str("session"),
            Self::Fast => f.write_str("fast"),
            Self::Named(name) => write!(f, "named:{name}"),
        }
    }
}

impl serde::Serialize for ModelRole {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}
impl ModelRole {
    fn from_word(mut role: String) -> Result<Self, String> {
        match role.as_str() {
            "session" => Ok(Self::Session),
            "fast" => Ok(Self::Fast),
            _ if role.starts_with("named:") => {
                role.drain(..6);
                Ok(Self::Named(role))
            }
            _ => Err("invalid model role".into()),
        }
    }
}
impl<'de> serde::Deserialize<'de> for ModelRole {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let role = <String as serde::Deserialize>::deserialize(d)?;
        Self::from_word(role).map_err(serde::de::Error::custom)
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

/// Where a session's messages go: the one extension that provides it
/// resolves each [`Address`](crate::Address) a send names to the sessions
/// that receive it, once, as the send commits, so a broadcast's recipients
/// are those of when it was sent. The engine only validates the issued
/// route and carries the envelopes; who may reach whom (parent and child
/// only, siblings directly, everything through the parent, a channel's
/// members) is the provider's.
pub trait Route: Send + Sync {
    fn resolve(
        &self,
        from: Topology<'_>,
        to: &crate::Address,
    ) -> Result<Recipients, protocol::InboxRefusal>;
}

/// The sessions a send reaches.
pub type Recipients = smallvec::SmallVec<[protocol::SessionId; 2]>;

/// Where the sending session stands among the sessions it may know of.
#[derive(Clone, Copy, Debug)]
pub struct Topology<'a> {
    pub session: protocol::SessionId,
    pub parent: Option<protocol::SessionId>,
    /// The children it linked, in order.
    pub children: &'a [protocol::SessionId],
}

/// An extension-supplied model transport for side requests, useful for
/// deterministic fixtures. It still runs behind the engine's log barriers.
pub trait SideProvider: Send + Sync {
    fn reply<'a>(
        &'a self,
        side: &'a crate::ModelTurn,
        messages: &'a llm::Messages,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>>;
}
