//! Services: typed capabilities one extension provides and others build
//! on, by type rather than by knowing each other. Each has at most one
//! provider per registry, and an extension that needs one no extension
//! provides is refused as it installs (or, in a static assembly, does not
//! compile). The engine provides the ones only it can, such as
//! [`Layers`], to every call.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::BoxFuture;
use protocol::{Environment, Held, Json};

use crate::{Called, Cx, DynTool, SpawnSpec, States};

/// Where a session's commands run. The shell provides it; what works
/// where they do (`edit`, the context the model is first given) asks it.
pub trait CwdOf: Send + Sync {
    /// The directory, for a session in `workspace` whose client's
    /// environment is `env`, with its extensions' `states`.
    fn dir(&self, workspace: &Path, env: Option<&Environment>, states: &States) -> PathBuf;
}

/// What a call's result is recorded as, asked once as the call ends and
/// before the engine logs it, and before the kernel's `post_tool` chain,
/// with the call's context: the one moment a result may be changed with
/// I/O (its payload moved into a blob it is then logged as), so what is
/// logged is the fact, and every projection of it stays pure. A call run
/// again is recorded again; a call another invoked is not recorded.
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
    /// A launch port that retains only runtime facilities. Shared callers supply
    /// declaration-owned cwd/env and an already admitted scope-owned spawn policy;
    /// this port never runs the first caller's session hooks.
    fn launcher(&self, cx: &Cx) -> Result<Arc<dyn ProcessLauncher>, SpawnError>;

    fn spawn<'a>(
        &'a self,
        cx: &'a Cx,
        launch: LaunchSpec,
    ) -> BoxFuture<'a, Result<OwnedProcess, SpawnError>>;

    /// `claimed`, a group the daemon's previous build handed over, owned
    /// again, its output going on into `output`.
    fn adopt<'a>(
        &'a self,
        _cx: &'a Cx,
        _claimed: Native,
        _output: std::fs::File,
    ) -> BoxFuture<'a, Result<OwnedProcess, SpawnError>> {
        Box::pin(std::future::ready(Err(
            std::io::ErrorKind::Unsupported.into()
        )))
    }
}

/// Session-free launch authority, supplied by the native process provider.
/// The owner's cancellation is independent of any individual request.
pub trait ProcessLauncher: Send + Sync {
    fn spawn(
        &self,
        cancel: crate::Cancel,
        launch: LaunchSpec,
    ) -> BoxFuture<'static, Result<OwnedProcess, SpawnError>>;
}

impl crate::ScopedResource for OwnedProcess {
    fn ended(&self) -> BoxFuture<'_, ()> {
        self.process.ended()
    }
    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = self.signal(ProcessSignal::Kill);
            self.process.ended().await;
        })
    }
}

/// A group stopped for a handover ([`RunningProcess::hand_over`]):
/// committed, it is given up whole, for good; dropped instead, its
/// supervision goes on here as if nothing happened.
pub trait Parting: Send {
    fn commit(self: Box<Self>) -> Given;
}

/// A group given up for the daemon's next build: where it finds it, as its
/// provider records it, and the handles it finds it by, open here until
/// the handover passes them on.
pub struct Given {
    pub group: Json,
    pub handles: Vec<std::fs::File>,
}

/// A process group claimed from the daemon's previous build, as it comes
/// to the provider that adopts it. What it holds is the platform's, opaque
/// to the SDK, which stays free of I/O; dropped, it is killed.
pub struct Native(Box<dyn std::any::Any + Send>);

impl Native {
    pub fn new(native: impl std::any::Any + Send) -> Self {
        Self(Box::new(native))
    }

    /// What it holds, if it is a `T`.
    pub fn take<T: std::any::Any>(self) -> Option<T> {
        self.0.downcast().ok().map(|native| *native)
    }
}

impl std::fmt::Debug for Native {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Native")
    }
}

/// What a process group the session owns is to it, as it says when it
/// takes the group ([`Cx::own`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The user's work: the session is at work while it runs, and an
    /// upgrade hands it over or waits for it to end.
    Work,
    /// A helper its extension starts again when next needed, such as an
    /// MCP server: it holds no session at work and no upgrade, which stops
    /// it.
    Helper,
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
    /// Stops the group's supervision for a handover to the daemon's next
    /// build, its output so far kept: see [`Parting`]. A group that ends
    /// meanwhile is not stopped, and the handover is refused.
    fn hand_over(&self) -> BoxFuture<'_, std::io::Result<Box<dyn Parting>>> {
        Box::pin(std::future::ready(Err(
            std::io::ErrorKind::Unsupported.into()
        )))
    }
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
        cx.own(self.process, Role::Work);
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
    holds: Mutex<Option<Holds>>,
}

/// What a group's end holds outside the log, made once the group has
/// ended: see [`ProcessLease::holding`].
pub type Holds = Box<dyn FnOnce() -> Vec<Held> + Send>;

impl ProcessLease {
    /// Completion and rejected adoption must both release dependent resources.
    #[must_use]
    pub fn on_release(mut self, released: impl FnOnce() + Send + Sync + 'static) -> Self {
        self.released = Some(Box::new(released));
        self
    }

    /// What the end of the group, once the session owns it, holds: made
    /// on a storage worker once nothing of the group is left and its
    /// output is drained (an output stream sealed now that nothing writes
    /// it), and dropped unmade if the session stops the group.
    #[must_use]
    pub fn holding(self, holds: impl FnOnce() -> Vec<Held> + Send + 'static) -> Self {
        *self.holds.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(holds));
        self
    }

    /// [`RunningProcess::hand_over`], for a group the session owns. Once
    /// committed, what its end holds is made by the next build, so here it
    /// is neither made nor dropped (dropping it would discard the stream
    /// the group still writes).
    pub fn hand_over(self: &Arc<Self>) -> BoxFuture<'static, std::io::Result<Box<dyn Parting>>> {
        let lease = Arc::clone(self);
        Box::pin(async move {
            let parting = lease.process.hand_over().await?;
            Ok(Box::new(Leased { lease, parting }) as Box<dyn Parting>)
        })
    }

    /// For the session that owns the group, as it ends: what
    /// [`ProcessLease::holding`] gave, once.
    pub fn holds(&self) -> Option<Holds> {
        self.holds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// A group the session owns, stopped for a handover.
struct Leased {
    lease: Arc<ProcessLease>,
    parting: Box<dyn Parting>,
}

impl Parting for Leased {
    fn commit(self: Box<Self>) -> Given {
        if let Some(holds) = self.lease.holds() {
            std::mem::forget(holds);
        }
        self.parting.commit()
    }
}

impl From<Arc<dyn RunningProcess>> for ProcessLease {
    fn from(process: Arc<dyn RunningProcess>) -> Self {
        Self {
            process,
            released: None,
            holds: Mutex::new(None),
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
/// resolves each [`Recipient`](crate::Recipient) a send names to the sessions
/// that receive it, once, as the send commits, so a broadcast's recipients
/// are those of when it was sent. The engine only validates the issued
/// route and carries the envelopes; who may reach whom (parent and child
/// only, siblings directly, everything through the parent, a channel's
/// members) is the provider's.
pub trait Route: Send + Sync {
    fn resolve(
        &self,
        from: Topology<'_>,
        to: &crate::Recipient,
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
