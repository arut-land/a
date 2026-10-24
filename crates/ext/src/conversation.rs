//! One primitive for every conversation a call owns: a title, a summary,
//! a judge's verdict and a sub-agent's task are all a [`Conversation`],
//! built by the extension that wants it and admitted by the engine.
//!
//! What differs between them is in the type, so an impossible combination
//! does not compile:
//! - the runner: a [`Turn`] is one model response with no tools; a
//!   [`Task`] runs a registered agent kind's loop with inherited tools;
//! - the storage: [`Persisted`] keeps an owned child log; an
//!   [`Ephemeral`] turn writes none, but is admitted, bounded and keyed
//!   in its owner's log like any other. Only a turn can be ephemeral: an
//!   agent loop needs a log to run its tools from;
//! - keyed reuse: a turn's [`TurnKey`] returns its committed result
//!   rather than asking again; a task is its call's, once per purpose;
//! - the context ([`Context`]) and, for a turn, its request bounds;
//! - the [`Placement`];
//! - awaited ([`Conversation::ask`]) or detached
//!   ([`Conversation::detach`]): a detached task answers with a durable
//!   [`ConversationHandle`], a detached turn with a [`Background`]
//!   observer. Neither has a result future.
//!
//! Each method sets only what is not the default, so an extension
//! declares what is different about its conversation and nothing else.

use std::marker::PhantomData;

use crate::{BoxFuture, Budget, Cx, Extension, ModelRole};
use protocol::{CallId, Outcome, SessionId};
use serde::{Deserialize, Serialize};

pub use llm::ContextPolicy;

/// A conversation an extension wants, until it is asked or detached.
/// Impossible compositions do not compile:
///
/// A task is never ephemeral: its loop runs tools from its own log.
/// ```compile_fail,E0599
/// # fn wrong<E: ext::Extension>() {
/// let _ = ext::Conversation::task::<E>("t", "p").ephemeral();
/// # }
/// ```
/// A task's result is its call's, so it takes no reuse key.
/// ```compile_fail,E0599
/// # fn wrong<E: ext::Extension>() {
/// let _ = ext::Conversation::task::<E>("t", "p").key("k");
/// # }
/// ```
/// A turn has no agent kind and no depth.
/// ```compile_fail,E0599
/// # fn wrong<E: ext::Extension>() {
/// let _ = ext::Conversation::turn::<E>("t", "p").kind("loop");
/// # }
/// ```
/// Ephemeral is a one-way choice.
/// ```compile_fail,E0599
/// # fn wrong<E: ext::Extension>() {
/// let _ = ext::Conversation::turn::<E>("t", "p").ephemeral().ephemeral();
/// # }
/// ```
/// Only a detached turn can be scoped to a question.
/// ```compile_fail,E0599
/// # fn wrong<E: ext::Extension>() {
/// let _ = ext::Conversation::turn::<E>("t", "p").while_waiting();
/// # }
/// ```
#[must_use]
pub struct Conversation<R, S = Persisted> {
    owner: &'static str,
    purpose: String,
    role: ModelRole,
    context: Context,
    budget: Budget,
    placement: Placement,
    runner: R,
    storage: PhantomData<fn() -> S>,
}

/// One model response per input, with no tools and no agent loop.
pub struct Turn {
    messages: llm::Messages,
    key: Option<TurnKey>,
    per_call: bool,
    bounds: ContextPolicy,
}

/// A registered agent kind's loop, with the tools its kind inherits.
pub struct Task {
    prompt: String,
    kind: Option<String>,
    depth: DepthBudget,
}

/// An owned child log: resumable, listable, and a turn's thread history.
pub enum Persisted {}
/// No child log; admission, bounds and keyed results stay in the owner's.
pub enum Ephemeral {}

/// What the conversation sees of its owner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Context {
    /// Only what the extension says.
    #[default]
    Fresh,
    /// The owner's effective conversation before the owning call, as an
    /// immutable image: a task's prefix, a turn's per-input context.
    Owner,
}

/// Where the conversation runs. Each daemon stores its own sessions, so a
/// conversation placed elsewhere will be a root there, owned by address,
/// rather than nested under its owner here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Placement {
    #[default]
    Here,
}

impl<R, S> Conversation<R, S> {
    fn with(owner: &'static str, purpose: &str, runner: R) -> Self {
        Self {
            owner,
            purpose: purpose.into(),
            role: ModelRole::Session,
            context: Context::Fresh,
            budget: Budget::default(),
            placement: Placement::Here,
            runner,
            storage: PhantomData,
        }
    }

    /// The model role it is routed by; the session's model by default.
    pub fn role(mut self, role: ModelRole) -> Self {
        self.role = role;
        self
    }
    /// Routed to the configured fast model.
    pub fn fast(self) -> Self {
        self.role(ModelRole::Fast)
    }
    /// Sees its owner's conversation before the owning call.
    pub fn owner_context(mut self) -> Self {
        self.context = Context::Owner;
        self
    }
    /// What each response, and the whole conversation's work, may take.
    pub fn budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }
    pub fn max_tokens(mut self, tokens: u32) -> Self {
        self.budget.max_tokens =
            std::num::NonZeroU32::new(tokens).unwrap_or(std::num::NonZeroU32::MIN);
        self
    }
    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }
}

impl Conversation<Turn> {
    /// A turn of `E`'s `purpose` thread, asking `prompt`. The thread is
    /// the session's, and the turn's key its owning call's.
    pub fn turn<E: Extension>(purpose: &str, prompt: impl Into<String>) -> Self {
        Self::with(
            E::NAME,
            purpose,
            Turn {
                messages: [llm::Message::user(prompt.into())].into_iter().collect(),
                key: None,
                per_call: false,
                bounds: Turn::BOUNDS,
            },
        )
    }

    /// Keeps no child log: no thread history, nothing to resume.
    pub fn ephemeral(self) -> Conversation<Turn, Ephemeral> {
        Conversation {
            owner: self.owner,
            purpose: self.purpose,
            role: self.role,
            context: self.context,
            budget: self.budget,
            placement: self.placement,
            runner: self.runner,
            storage: PhantomData,
        }
    }
}

impl Turn {
    const BOUNDS: ContextPolicy = ContextPolicy {
        exchanges: 8,
        bytes: 65536,
    };
}

impl<S: Storage> Conversation<Turn, S> {
    /// System instructions ahead of the prompt.
    pub fn instructions(mut self, system: &str) -> Self {
        self.runner.messages = std::iter::once(llm::Message::system(system))
            .chain(self.runner.messages.iter().cloned())
            .collect();
        self
    }
    /// The key a repeated request reuses the committed result of.
    pub fn key(mut self, key: &str) -> Self {
        self.runner.key = Some(TurnKey(key.into()));
        self
    }
    /// A thread of the owning call's rather than the session's.
    pub fn per_call(mut self) -> Self {
        self.runner.per_call = true;
        self
    }
    /// The recent exchanges and serialized bytes a request may carry.
    pub fn bounds(mut self, exchanges: u16, bytes: u32) -> Self {
        self.runner.bounds = ContextPolicy { exchanges, bytes };
        self
    }

    /// The admitted form of this turn, as the call `call` asks it.
    #[doc(hidden)]
    pub fn request(self, call: CallId) -> ModelTurn {
        ModelTurn {
            thread: ThreadKey {
                extension: self.owner.into(),
                purpose: self.purpose,
                owner_scope: if self.runner.per_call {
                    OwnerScope::Call(call)
                } else {
                    OwnerScope::Session
                },
            },
            turn: (self.runner.key).unwrap_or_else(|| TurnKey(call.get().to_string())),
            role: self.role,
            messages: self.runner.messages,
            context: self.context,
            budget: self.budget,
            background: false,
            while_waiting: false,
            ephemeral: S::EPHEMERAL,
            bounds: self.runner.bounds,
        }
    }

    /// Returns only after the result commits. A lost attempt is
    /// interrupted, never repeated by recovery.
    pub async fn ask(self, cx: &Cx) -> Result<llm::Response, llm::Error> {
        let Some(conversations) = cx.conversations() else {
            return Err(llm::Error::new(
                llm::Kind::Other,
                "this engine has no conversations",
            ));
        };
        conversations.turn(cx, self.request(cx.call)).await
    }

    /// Observation work: completion is a logged fact, never a reply.
    pub fn detach(self) -> Background {
        Background {
            turn: Box::new(move |call| {
                let mut turn = self.request(call);
                turn.background = true;
                turn
            }),
            while_waiting: false,
        }
    }
}

/// Whether a conversation keeps a child log. Sealed: these two are all.
pub trait Storage: sealed::Sealed + 'static {
    #[doc(hidden)]
    const EPHEMERAL: bool;
}
impl Storage for Persisted {
    const EPHEMERAL: bool = false;
}
impl Storage for Ephemeral {
    const EPHEMERAL: bool = true;
}
mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Persisted {}
    impl Sealed for super::Ephemeral {}
}

impl Conversation<Task> {
    /// `E`'s `purpose` child running `prompt` under the current agent kind.
    pub fn task<E: Extension>(purpose: &str, prompt: impl Into<String>) -> Self {
        Self::with(
            E::NAME,
            purpose,
            Task {
                prompt: prompt.into(),
                kind: None,
                depth: Task::DEPTH,
            },
        )
    }
    /// A registered agent kind other than the current one.
    pub fn kind(mut self, kind: impl Into<String>) -> Self {
        self.runner.kind = Some(kind.into());
        self
    }
    /// How many more ownership edges may descend from it.
    pub fn depth(mut self, edges: u16) -> Self {
        self.runner.depth = DepthBudget::new(edges);
        self
    }

    /// The admitted form of this task, under agent kind `current` unless
    /// it names another.
    #[doc(hidden)]
    pub fn spawn(self, current: impl FnOnce() -> String) -> Spawn {
        Spawn {
            owner: self.owner.into(),
            purpose: self.purpose,
            prompt: self.runner.prompt,
            context: self.context,
            runner: Runner::Agent {
                agent: self.runner.kind.unwrap_or_else(current),
                role: self.role,
            },
            depth: self.runner.depth,
            budget: self.budget,
            placement: self.placement,
        }
    }

    /// What the child settles with. The child is the call's own: started
    /// once however often the call runs again (as recovery runs it), and
    /// heard from once. An interrupt of the call's turn reaches it first.
    pub async fn ask(self, cx: &Cx) -> Result<Outcome, SpawnRefusal> {
        let conversations = cx.conversations().ok_or_else(Self::unavailable)?;
        let spawn = self.spawn(|| conversations.kind());
        conversations.task(cx, spawn).await
    }

    /// A durable handle once the child is admitted; it keeps running after
    /// the call returns, and its result arrives later.
    pub async fn detach(self, cx: &Cx) -> Result<ConversationHandle, SpawnRefusal> {
        let conversations = cx.conversations().ok_or_else(Self::unavailable)?;
        let spawn = self.spawn(|| conversations.kind());
        conversations.detach(cx, spawn).await
    }

    fn unavailable() -> SpawnRefusal {
        SpawnRefusal::Unavailable("this engine runs no child sessions".into())
    }
}

impl Task {
    const DEPTH: DepthBudget = DepthBudget::new(8);
}

/// A detached turn: optional work whose completion is logged, and which
/// offers no result to wait on.
///
/// ```compile_fail,E0599
/// # async fn awaited(cx: &ext::Cx, background: ext::Background) {
/// let result = background.ask(cx).await;
/// # }
/// ```
#[must_use]
pub struct Background {
    turn: Box<dyn FnOnce(CallId) -> ModelTurn + Send>,
    while_waiting: bool,
}

impl Background {
    /// Scope optional work to the owner's current question. Answering the
    /// question cancels it even when the approved owner keeps running.
    pub fn while_waiting(mut self) -> Self {
        self.while_waiting = true;
        self
    }
    /// Shows the reply on the owning call for `lifetime`.
    pub fn annotate(self, cx: Cx, lifetime: protocol::AnnotationLifetime) -> crate::Reaction {
        self.finish(cx, Some(lifetime))
    }

    pub fn start(self, cx: Cx) -> crate::Reaction {
        self.finish(cx, None)
    }

    fn finish(self, cx: Cx, annotation: Option<protocol::AnnotationLifetime>) -> crate::Reaction {
        let mut turn = (self.turn)(cx.call);
        turn.while_waiting = self.while_waiting;
        crate::Reaction::later(crate::Requirement::Ordered, async move {
            if let Some(conversations) = cx.conversations()
                && let Ok(reply) = conversations.turn(&cx, turn).await
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

/// A durable address has no future or polling semantics.
/// ```compile_fail
/// async fn wrong(handle: ext::ConversationHandle) { handle.await; }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationHandle {
    pub session: SessionId,
}

impl ConversationHandle {
    /// Sends `payload` to the conversation as `E`'s `purpose`, which with
    /// the call keeps the message's identity across retries.
    pub async fn send<E: Extension>(
        self,
        cx: &Cx,
        purpose: &str,
        payload: String,
    ) -> Result<(), protocol::InboxRefusal> {
        let conversations = cx
            .conversations()
            .ok_or(protocol::InboxRefusal::Unauthorized)?;
        let message = SendMessage {
            owner: E::NAME,
            purpose: purpose.into(),
            destination: self,
            payload,
        };
        conversations.send(cx, message).await
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnRefusal {
    DepthExhausted,
    Unavailable(String),
    Interrupted,
}

/// The engine's side of [`Conversation`]: it admits, runs and settles
/// each kind. Extensions reach it only through the typed requests.
pub trait Conversations: Send + Sync {
    /// The agent kind a task keeps when it names none.
    fn kind(&self) -> String {
        crate::LOOP.into()
    }
    fn turn<'a>(
        &'a self,
        cx: &'a Cx,
        turn: ModelTurn,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>>;
    fn task<'a>(&'a self, _: &'a Cx, _: Spawn) -> BoxFuture<'a, Result<Outcome, SpawnRefusal>> {
        Box::pin(std::future::ready(Err(SpawnRefusal::Unavailable(
            "no child sessions here".into(),
        ))))
    }
    fn detach<'a>(
        &'a self,
        _: &'a Cx,
        _: Spawn,
    ) -> BoxFuture<'a, Result<ConversationHandle, SpawnRefusal>> {
        Box::pin(std::future::ready(Err(SpawnRefusal::Unavailable(
            "no child sessions here".into(),
        ))))
    }
    fn send<'a>(
        &'a self,
        _: &'a Cx,
        _: SendMessage,
    ) -> BoxFuture<'a, Result<(), protocol::InboxRefusal>> {
        Box::pin(std::future::ready(Err(
            protocol::InboxRefusal::Unauthorized,
        )))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[must_use]
pub struct DepthBudget(u16);

impl DepthBudget {
    pub const fn new(edges: u16) -> Self {
        Self(edges)
    }
    pub const fn remaining(self) -> u16 {
        self.0
    }
    pub const fn descend(self) -> Result<Self, SpawnRefusal> {
        match self.0.checked_sub(1) {
            Some(edges) => Ok(Self(edges)),
            None => Err(SpawnRefusal::DepthExhausted),
        }
    }
    pub const fn intersect(self, other: Self) -> Self {
        Self(if self.0 < other.0 { self.0 } else { other.0 })
    }
}

/// What runs an admitted conversation, frozen with it.
#[derive(Clone, Debug)]
pub enum Runner {
    Agent {
        agent: String,
        role: ModelRole,
    },
    ModelOnly {
        role: ModelRole,
        context: ContextPolicy,
    },
}

impl Serialize for Runner {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        (self.agent(), self.role(), self.context_policy()).serialize(s)
    }
}
impl<'de> Deserialize<'de> for Runner {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (agent, role, context): (Option<String>, ModelRole, Option<ContextPolicy>) =
            Deserialize::deserialize(d)?;
        Ok(match agent {
            Some(agent) => Self::Agent { agent, role },
            None => Self::ModelOnly {
                role,
                context: context
                    .ok_or_else(|| serde::de::Error::custom("missing model-only context policy"))?,
            },
        })
    }
}

impl Runner {
    pub fn context_policy(&self) -> Option<ContextPolicy> {
        match self {
            Self::Agent { .. } => None,
            Self::ModelOnly { context, .. } => Some(*context),
        }
    }
    pub fn agent(&self) -> Option<&str> {
        match self {
            Self::Agent { agent, .. } => Some(agent),
            Self::ModelOnly { .. } => None,
        }
    }
    pub fn role(&self) -> &ModelRole {
        match self {
            Self::Agent { role, .. } | Self::ModelOnly { role, .. } => role,
        }
    }
}

/// An admitted child's public request, frozen in its spawn intent. Only a
/// [`Conversation`] makes one.
#[derive(Clone, Debug)]
pub struct Spawn {
    owner: String,
    pub purpose: String,
    pub prompt: String,
    pub context: Context,
    pub runner: Runner,
    pub depth: DepthBudget,
    pub budget: Budget,
    pub placement: Placement,
}

impl Spawn {
    pub fn to_json(&self) -> serde_json::Result<protocol::Json> {
        protocol::Json::encode(&(
            &self.owner,
            &self.purpose,
            &self.prompt,
            self.context,
            &self.runner,
            self.depth,
            self.budget,
            self.placement,
        ))
    }

    pub fn from_json(json: &protocol::Json) -> serde_json::Result<Self> {
        let (owner, purpose, prompt, context, runner, depth, budget, placement) = json.parse()?;
        Ok(Self {
            owner,
            purpose,
            prompt,
            context,
            runner,
            depth,
            budget,
            placement,
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }
}

impl From<&ModelTurn> for Spawn {
    fn from(turn: &ModelTurn) -> Self {
        Self {
            owner: turn.thread.extension.clone(),
            purpose: turn.thread.purpose.clone(),
            prompt: String::new(),
            context: Context::Fresh,
            runner: Runner::ModelOnly {
                role: turn.role.clone(),
                context: turn.bounds,
            },
            depth: DepthBudget::new(u16::MAX),
            budget: turn.budget,
            placement: Placement::Here,
        }
    }
}

/// A stable owned conversation, independent of any one provider turn.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadKey {
    pub extension: String,
    pub purpose: String,
    pub owner_scope: OwnerScope,
}

impl ThreadKey {
    pub fn conversation(&self, owner: SessionId) -> SessionId {
        owner.owned(llm::Json::encode(self).expect("thread key encodes").raw())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OwnerScope {
    Session,
    Call(CallId),
}

impl Serialize for OwnerScope {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Session => None,
            Self::Call(call) => Some(*call),
        }
        .serialize(s)
    }
}
impl<'de> Deserialize<'de> for OwnerScope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        <Option<CallId>>::deserialize(d).map(|call| call.map_or(Self::Session, Self::Call))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnKey(pub String);

/// What a turn's committed result is found by.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
pub struct SideIdentity {
    pub thread: ThreadKey,
    pub turn: TurnKey,
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

/// An admitted turn, as its owner logs it. Only a [`Conversation`] (or,
/// for typed input into a thread, the engine) makes one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelTurn {
    pub thread: ThreadKey,
    pub turn: TurnKey,
    pub role: ModelRole,
    pub messages: llm::Messages,
    pub context: Context,
    pub budget: Budget,
    background: bool,
    while_waiting: bool,
    ephemeral: bool,
    bounds: ContextPolicy,
}

impl ModelTurn {
    /// Text typed into model-only thread `spawn` as its call `call`.
    #[doc(hidden)]
    pub fn typed(spawn: &Spawn, call: CallId, text: &str) -> Option<Self> {
        Some(Self {
            thread: ThreadKey {
                extension: spawn.owner.clone(),
                purpose: spawn.purpose.clone(),
                owner_scope: OwnerScope::Session,
            },
            turn: TurnKey(call.get().to_string()),
            role: spawn.runner.role().clone(),
            messages: [llm::Message::user(text)].into_iter().collect(),
            context: Context::Fresh,
            budget: spawn.budget,
            background: false,
            while_waiting: false,
            ephemeral: false,
            bounds: spawn.runner.context_policy()?,
        })
    }

    pub fn from_intent(text: &str) -> serde_json::Result<(Self, Option<protocol::Seq>)> {
        serde_json::from_slice(text.as_bytes())
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

    pub const fn bounds(&self) -> ContextPolicy {
        self.bounds
    }
    pub const fn is_while_waiting(&self) -> bool {
        self.while_waiting
    }
    pub const fn is_background(&self) -> bool {
        self.background
    }
    pub const fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }
}

/// A sender chooses an extension-local purpose; the engine supplies its call id.
#[derive(Clone, Debug)]
pub struct SendMessage {
    owner: &'static str,
    purpose: String,
    destination: ConversationHandle,
    payload: String,
}

impl SendMessage {
    pub fn envelope(&self, sender: SessionId, call: CallId) -> protocol::Envelope {
        protocol::Envelope {
            id: protocol::EnvelopeId {
                sender,
                call,
                purpose: format!("{}:{}", self.owner, self.purpose),
            },
            destination: self.destination.session,
            payload: self.payload.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Owner;
    impl Extension for Owner {
        const NAME: &'static str = "identity";
        const VERSION: u32 = 1;
        fn contribute(&self, _: &mut crate::Registrar) {}
    }

    #[test]
    fn a_turns_identity_depends_only_on_extension_purpose_scope_and_key() {
        let of = CallId::new(1).unwrap();
        let other = CallId::new(2).unwrap();
        let turn = || Conversation::turn::<Owner>("summary", "prompt").per_call();
        let identity = turn().key("initial").request(of).identity();
        let reworded = Conversation::turn::<Owner>("summary", "prompt".repeat(1 << 18))
            .per_call()
            .key("initial")
            .owner_context()
            .fast()
            .max_tokens(1)
            .ephemeral()
            .request(of);
        assert_eq!(reworded.identity(), identity);
        assert_ne!(turn().key("initial").request(other).identity(), identity);
        assert_ne!(turn().key("next").request(of).identity(), identity);
        assert_ne!(turn().request(of).identity(), identity);
        let session = Conversation::turn::<Owner>("summary", "prompt")
            .key("initial")
            .request(of);
        assert_ne!(session.identity(), identity);
        assert_eq!(
            session.identity(),
            Conversation::turn::<Owner>("summary", "prompt")
                .key("initial")
                .request(other)
                .identity()
        );
        assert_ne!(
            Conversation::turn::<Owner>("other", "prompt")
                .per_call()
                .key("initial")
                .request(of)
                .identity(),
            identity
        );
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

    #[test]
    fn only_a_turn_can_be_ephemeral_and_it_says_so_in_its_admitted_form() {
        let call = CallId::new(3).unwrap();
        let kept = Conversation::turn::<Owner>("t", "p").request(call);
        let dropped = Conversation::turn::<Owner>("t", "p")
            .ephemeral()
            .request(call);
        assert!(!kept.is_ephemeral());
        assert!(dropped.is_ephemeral());
        let (restored, _) = ModelTurn::from_intent(dropped.intent(None).unwrap().raw()).unwrap();
        assert!(restored.is_ephemeral());
    }

    #[test]
    fn a_task_keeps_the_current_kind_unless_it_names_one() {
        let kept = Conversation::task::<Owner>("t", "p").spawn(|| "loop".into());
        assert_eq!(kept.runner.agent(), Some("loop"));
        let named = Conversation::task::<Owner>("t", "p")
            .kind("reviewer")
            .spawn(|| unreachable!("a named kind needs no default"));
        assert_eq!(named.runner.agent(), Some("reviewer"));
        let restored = Spawn::from_json(&named.to_json().unwrap()).unwrap();
        assert_eq!(restored.placement, Placement::Here);
        assert_eq!(restored.owner(), "identity");
    }

    #[test]
    fn context_bounds_keep_instructions_and_whole_exchanges() {
        let current: llm::Messages = [llm::Message::user("current instruction")]
            .into_iter()
            .collect();
        let history: llm::Messages = [
            llm::Message::system("thread system"),
            llm::Message::user("old instruction"),
            llm::Message::assistant("old reply"),
            llm::Message::user("recent instruction"),
            llm::Message::assistant("recent reply"),
        ]
        .into_iter()
        .collect();
        let owner: llm::Messages = [
            llm::Message::system("owner system"),
            llm::Message::user("x".repeat(4096)),
        ]
        .into_iter()
        .collect();
        let policy = ContextPolicy {
            exchanges: 1,
            bytes: 512,
        };
        let assembled = policy.assemble(&history, &owner, &current).unwrap();
        assert_eq!(
            assembled,
            [
                llm::Message::user("recent instruction"),
                llm::Message::assistant("recent reply"),
                llm::Message::user("current instruction")
            ]
            .into_iter()
            .collect()
        );
        assert!(protocol::Json::encode(&assembled).unwrap().raw().len() <= 512);
        assert!(
            ContextPolicy {
                exchanges: 1,
                bytes: 1
            }
            .assemble(&history, &owner, &current)
            .is_err()
        );
    }

    #[test]
    fn descending_and_intersection_never_increase_allowance() {
        for parent in 0..32 {
            for requested in 0..32 {
                let budget = DepthBudget::new(parent).intersect(DepthBudget::new(requested));
                match budget.descend() {
                    Ok(child) => {
                        assert!(child.remaining() < parent);
                        assert!(child.remaining() < requested);
                    }
                    Err(error) => {
                        assert_eq!(error, SpawnRefusal::DepthExhausted);
                        assert!(parent == 0 || requested == 0);
                    }
                }
            }
        }
    }
    #[test]
    fn persisted_depth_cannot_replenish_itself() {
        let depth = DepthBudget::new(2).descend().unwrap();
        let restored: DepthBudget = crate::Json::encode(&depth).unwrap().parse().unwrap();
        assert_eq!(
            restored.descend().unwrap().descend(),
            Err(SpawnRefusal::DepthExhausted)
        );
    }
}
