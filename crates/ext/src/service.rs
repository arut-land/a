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
use protocol::{Environment, Outcome};

use crate::{Cx, SpawnSpec, States};

/// Where a session's commands run. The shell provides it; what works
/// where they do (`edit`, the context the model is first given) asks it.
pub trait CwdOf: Send + Sync {
    /// The directory, for a session in `workspace` whose client's
    /// environment is `env`, with its extensions' `states`.
    fn dir(&self, workspace: &Path, env: Option<&Environment>, states: &States) -> PathBuf;
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
    fn ended(&self) -> BoxFuture<'_, ()>;
}

pub trait ProcessPipe: Send + Sync {
    fn read<'a>(&'a self, bytes: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>>;
    fn write<'a>(&'a self, bytes: &'a [u8]) -> BoxFuture<'a, std::io::Result<usize>>;
    fn try_write(&self, bytes: &[u8]) -> std::io::Result<usize>;
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

/// Model requests owned by a session, outside its main transcript.
pub trait Models: Send + Sync {
    /// Reuse the committed fact for this extension's key, or admit it and
    /// return only after its result commits. A lost attempt is interrupted,
    /// never repeated by recovery. The engine provides this service.
    fn call<'a>(
        &'a self,
        cx: &'a Cx,
        side: ModelTurn,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>>;
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

/// The immutable transcript prefix visible to a side request.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum SideContext {
    None,
    /// The effective conversation strictly before the owning call.
    BeforeOwner,
}

/// A stable owned conversation, independent of any one provider turn.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ThreadKey {
    pub extension: String,
    pub purpose: String,
    pub owner_scope: OwnerScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OwnerScope {
    Session,
    Call(protocol::CallId),
}

impl serde::Serialize for OwnerScope {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(
            &match self {
                Self::Session => None,
                Self::Call(call) => Some(*call),
            },
            s,
        )
    }
}
impl<'de> serde::Deserialize<'de> for OwnerScope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        <Option<protocol::CallId> as serde::Deserialize>::deserialize(d)
            .map(|call| call.map_or(Self::Session, Self::Call))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TurnKey(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
pub struct SideIdentity {
    pub thread: ThreadKey,
    pub turn: TurnKey,
}

impl ThreadKey {
    pub fn conversation(&self, owner: protocol::SessionId) -> protocol::SessionId {
        owner.owned(llm::Json::encode(self).expect("thread key encodes").raw())
    }

    pub fn new<E: crate::Extension>(purpose: &str, owner_scope: OwnerScope) -> Self {
        Self {
            extension: E::NAME.into(),
            purpose: purpose.into(),
            owner_scope,
        }
    }
}

impl SideIdentity {
    pub fn to_json(&self) -> serde_json::Result<llm::Json> {
        llm::Json::encode(&(&self.thread, &self.turn))
    }

    pub fn from_intent(intent: &llm::Json) -> serde_json::Result<Self> {
        let image = protocol::publication::ThreadImage::from_json(intent)?;
        let (identity, _): (Self, serde::de::IgnoredAny) = image.turn.parse()?;
        Ok(identity)
    }
}

/// A keyed response in an owned conversation, outside its parent's transcript.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelTurn {
    pub thread: ThreadKey,
    pub turn: TurnKey,
    pub role: ModelRole,
    pub messages: llm::Messages,
    pub context: SideContext,
    pub budget: Budget,
    background: bool,
    while_waiting: bool,
    context_policy: crate::ContextPolicy,
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
        match self {
            Self::Session => s.serialize_str("session"),
            Self::Fast => s.serialize_str("fast"),
            Self::Named(name) => s.collect_str(&format_args!("named:{name}")),
        }
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

impl ModelTurn {
    pub fn from_intent(text: &str) -> serde_json::Result<(Self, Option<protocol::Seq>)> {
        serde_json::from_slice(text.as_bytes())
    }

    pub fn context_policy(&self) -> crate::ContextPolicy {
        self.context_policy
    }

    pub fn identity(&self) -> SideIdentity {
        SideIdentity {
            thread: self.thread.clone(),
            turn: self.turn.clone(),
        }
    }

    pub fn intent(&self, prefix: Option<protocol::Seq>) -> Result<llm::Json, String> {
        llm::Json::encode(&(self, prefix)).map_err(|e| e.to_string())
    }

    pub const fn is_while_waiting(&self) -> bool {
        self.while_waiting
    }
    pub const fn is_background(&self) -> bool {
        self.background
    }

    pub fn new<K: crate::ModelOnlyKind>(
        thread: ThreadKey,
        turn: TurnKey,
        role: ModelRole,
        context: SideContext,
        messages: llm::Messages,
    ) -> Self {
        Self {
            thread,
            turn,
            role,
            messages,
            context,
            budget: Budget::default(),
            background: false,
            while_waiting: false,
            context_policy: K::CONTEXT,
        }
    }

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
pub struct Background(ModelTurn);

impl Background {
    /// Scope optional work to the owner's current question. Answering the
    /// question cancels it even when the approved owner keeps running.
    #[must_use]
    pub fn while_waiting(mut self) -> Self {
        self.0.while_waiting = true;
        self
    }
    pub fn annotate(self, cx: Cx, lifetime: protocol::AnnotationLifetime) -> crate::Reaction {
        self.finish(cx, Some(lifetime))
    }

    pub fn start(self, cx: Cx) -> crate::Reaction {
        self.finish(cx, None)
    }

    fn finish(self, cx: Cx, annotation: Option<protocol::AnnotationLifetime>) -> crate::Reaction {
        crate::Reaction::later(crate::Requirement::Ordered, async move {
            if let Some(models) = cx.models()
                && let Ok(reply) = models.call(&cx, self.0).await
                && let Some(lifetime) = annotation
            {
                let body = reply.message.text().trim().to_owned();
                if !body.is_empty() {
                    let _ = cx
                        .fact(protocol::ExtensionFact::Annotation(protocol::Annotation {
                            target: cx.call,
                            lifetime,
                            card: protocol::Card {
                                body,
                                ..protocol::Card::default()
                            },
                        }))
                        .await;
                }
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
/// An extension chooses which ownership edges may carry messages.
pub trait InboxAccess: Send + Sync {
    fn permits(
        &self,
        destination: protocol::SessionId,
        parent: Option<protocol::SessionId>,
        children: &[protocol::SessionId],
    ) -> bool;
}

pub trait Sessions: Send + Sync {
    fn runner(&self) -> crate::Runner;
    fn send<'a>(
        &'a self,
        cx: &'a Cx,
        message: crate::SendMessage,
    ) -> BoxFuture<'a, Result<(), protocol::InboxRefusal>>;
    fn detach<'a>(
        &'a self,
        cx: &'a Cx,
        request: crate::Detached<crate::Spawn>,
    ) -> BoxFuture<'a, Result<crate::ConversationHandle, crate::SpawnRefusal>>;
    /// What a child session started on `input` settles with. The child is
    /// the call's own: started once however often the call runs again (as
    /// recovery runs it), and heard from once. An interrupt of the call's
    /// turn reaches the child first.
    fn spawn<'a>(
        &'a self,
        cx: &'a Cx,
        request: crate::Awaited<crate::Spawn>,
    ) -> BoxFuture<'a, Result<Outcome, crate::SpawnRefusal>>;
}

/// An extension-supplied model transport for side requests, useful for
/// deterministic fixtures. It still runs behind the engine's log barriers.
pub trait SideProvider: Send + Sync {
    fn reply<'a>(
        &'a self,
        side: &'a ModelTurn,
        messages: &'a llm::Messages,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Owner;
    impl crate::Extension for Owner {
        const NAME: &'static str = "identity";
        const VERSION: u32 = 1;
        fn contribute(&self, _: &mut crate::Registrar) {}
    }

    #[test]
    fn side_identity_depends_only_on_extension_purpose_and_owner_scope() {
        let of = protocol::CallId::new(1).unwrap();
        let other = protocol::CallId::new(2).unwrap();
        let mut side = ModelTurn::new::<crate::ModelOnly>(
            ThreadKey::new::<Owner>("summary", OwnerScope::Call(of)),
            TurnKey("initial".into()),
            ModelRole::Fast,
            SideContext::None,
            llm::Messages::default(),
        );
        let identity = side.identity();
        side.messages
            .push(llm::Message::user("prompt".repeat(1 << 18)));
        side.context = SideContext::BeforeOwner;
        side.role = ModelRole::Session;
        assert_eq!(side.identity(), identity);
        side.thread.owner_scope = OwnerScope::Call(other);
        assert_ne!(side.identity(), identity);
        side.thread.owner_scope = OwnerScope::Call(of);
        let mut renamed = side.clone();
        renamed.thread.extension = "another-extension".into();
        assert_ne!(renamed.identity(), identity);
        renamed = side.clone();
        renamed.thread.purpose = "another-purpose".into();
        assert_ne!(renamed.identity(), identity);
        renamed = side.clone();
        renamed.turn = TurnKey("next".into());
        assert_ne!(renamed.identity(), identity);
        let mut session = side;
        session.thread.owner_scope = OwnerScope::Session;
        assert_ne!(session.identity(), identity);
        let intent = protocol::publication::ThreadImage {
            turn: session.intent(None).unwrap(),
            authority: llm::Json::null(),
        }
        .to_json()
        .unwrap();
        assert_eq!(
            SideIdentity::from_intent(&intent).unwrap(),
            session.identity()
        );
    }
}
