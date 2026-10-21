//! Typed tools, the erased form a toolset holds, and what a call runs
//! with.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use protocol::{Audience, CallId, Card, Environment, Json, Outcome, Seq, SessionId};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::BoxFuture;
#[cfg(doc)]
use crate::CwdOf;
use crate::flag::Cancel;
use crate::host::{Host, Services};
use crate::point::{PointCx, View};
use crate::schema::Schema;
use crate::shape::Output;
#[cfg(doc)]
use crate::shape::Streamed;
use crate::state::{self, Fold, States};

/// The durable entry acknowledging an accepted mutation.
pub type MutationResult = Result<Seq, MutationError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationError {
    Revoked,
    Unregistered,
    Rejected(String),
    Encoding(String),
    CommitFailed,
}

impl fmt::Display for MutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revoked => f.write_str("mutation authority revoked"),
            Self::Unregistered => f.write_str("mutation state is not registered"),
            Self::Rejected(error) | Self::Encoding(error) => f.write_str(error),
            Self::CommitFailed => f.write_str("mutation was not durably committed"),
        }
    }
}

impl std::error::Error for MutationError {}

/// Whether a call interrupted by a crash may run again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Replay {
    Safe,
    #[default]
    Unsafe,
}

/// Whether a call runs alongside the other calls of its turn. The calls
/// of one reply start together while each is shared; an exclusive one
/// waits until none runs, and those after it wait for it, so it never
/// overlaps another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Concurrency {
    /// It changes nothing another call reads, or works elsewhere.
    Shared,
    /// It may change what another call reads: files, the working
    /// directory.
    #[default]
    Exclusive,
}

/// What a user's command is given: the text after its name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Args {
    #[serde(default)]
    pub args: String,
}

impl Schema for Args {
    fn schema(out: &mut String) {
        out.push_str(r#"{"type":"object","properties":{"args":{"type":"string"}},"required":[],"additionalProperties":false}"#);
    }
}

/// What the kernel knows of a callable: who may call it, whether a call a
/// crash cut off runs again, and whether it runs alongside others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Callable {
    pub audience: Audience,
    pub replay: Replay,
    pub concurrency: Concurrency,
}

/// A process a tool starts: `argv[0]` is the program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    pub argv: Vec<String>,
    /// Exactly the variables it runs with.
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

/// What a tool or command call runs with. Cheap to clone, and owned, so
/// work the call hands off (to a blocking thread, a background task) can
/// take it along. Only the engine makes one: see [`host::Parts`](crate::host::Parts).
#[derive(Clone)]
pub struct Cx {
    /// The session's workspace.
    pub workspace: Arc<Path>,
    /// The attaching client's environment, which processes run in. `None`
    /// (a session no client has described) inherits the daemon's.
    pub env: Option<Arc<Environment>>,
    pub session: SessionId,
    /// The call's id: the seq of its intent, and the key a call run again
    /// (as recovery runs a safe one) is told apart by.
    pub call: CallId,
    /// The model the session talks to, as `host:model`.
    pub model: Arc<str>,
    /// The session's directory for output too large for the log, created
    /// on first use.
    pub spill: Arc<Path>,
    pub cancel: Cancel,
    services: Arc<Services>,
    host: Arc<dyn Host>,
    delegated: u8,
}

impl Cx {
    pub(crate) fn new(call: crate::host::Parts, host: Arc<dyn Host>) -> Self {
        let crate::host::Parts {
            workspace,
            env,
            session,
            call,
            model,
            spill,
            cancel,
            services,
        } = call;
        Self {
            workspace,
            env,
            session,
            call,
            model,
            spill,
            cancel,
            services,
            host,
            delegated: 0,
        }
    }

    /// Reads state `S`, or its default when no extension registered it.
    pub fn state<S: Fold, R>(&self, read: impl FnOnce(&S) -> R) -> R {
        state::read(|f| self.host.states(f), read)
    }

    /// Commits `change` to state `S`: it resolves once the change is
    /// committed and folded in, so a read after success sees it.
    pub fn update<S: Fold>(&self, change: &S::Change) -> BoxFuture<'static, MutationResult> {
        let mut ext = None;
        self.host
            .states(&mut |states| ext = states.ext_of::<S>().cloned());
        let Some(ext) = ext else {
            return Box::pin(std::future::ready(Err(MutationError::Unregistered)));
        };
        match Json::encode(change) {
            Ok(data) => self.host.note(&ext, S::NAME, data),
            Err(e) => Box::pin(std::future::ready(Err(MutationError::Encoding(
                e.to_string(),
            )))),
        }
    }

    /// Records bookkeeping for an effect whose result already exists.
    /// Revocation prevents the record, but cannot undo the effect or replace
    /// its result. Use `update` instead when committing state is the operation.
    pub async fn record_effect<S: Fold, T>(
        &self,
        output: T,
        change: &S::Change,
    ) -> Result<T, MutationError> {
        match self.update::<S>(change).await {
            Ok(_) | Err(MutationError::Revoked) => Ok(output),
            Err(error) => Err(error),
        }
    }

    /// Seals a typed publication key and its model context in one log entry.
    pub fn publish<P: crate::Publication>(
        &self,
        key: &P::Key,
        text: String,
    ) -> BoxFuture<'static, MutationResult> {
        match Json::encode(key) {
            Ok(key) => self.publish_erased(P::NAME, key, text),
            Err(error) => Box::pin(std::future::ready(Err(MutationError::Encoding(
                error.to_string(),
            )))),
        }
    }

    pub fn published<P: crate::Publication>(&self, key: &P::Key) -> bool {
        let Ok(key) = Json::encode(key) else {
            return false;
        };
        self.published_erased(P::NAME, &key)
    }

    pub fn publish_erased(
        &self,
        namespace: &str,
        key: Json,
        text: String,
    ) -> BoxFuture<'static, MutationResult> {
        self.host.publish(namespace, key, text)
    }

    pub fn published_erased(&self, namespace: &str, key: &Json) -> bool {
        let mut published = false;
        self.host
            .states(&mut |states| published = states.publications().contains(namespace, key));
        published
    }

    /// Reads a fold named at run time, as JSON.
    pub fn state_erased(&self, ext: &str, name: &str) -> Option<Json> {
        let mut value = None;
        self.host
            .states(&mut |states| value = states.erased(ext, name).cloned());
        value
    }

    /// Commits a change to a registered fold named at run time.
    pub fn update_erased(
        &self,
        ext: &str,
        name: &str,
        change: Json,
    ) -> BoxFuture<'static, MutationResult> {
        if self.state_erased(ext, name).is_some() {
            self.host.note(ext, name, change)
        } else {
            Box::pin(std::future::ready(Err(MutationError::Unregistered)))
        }
    }

    /// Runs a delegated tool through this generation's policy. An ask
    /// without an extension's approval remains refused; delegation cannot
    /// grant itself a permission the outer call did not have.
    pub async fn invoke(&self, tool: &str, input: &Json) -> Called {
        use crate::{Gate, Gating};
        if self.delegated >= 8 {
            return Called::failed("tool delegation is too deep".into());
        }
        let mut child = self.clone();
        child.delegated += 1;
        let registry = self.host.registry();
        let gated = match registry.hooks().gate(&self.point(), tool, input) {
            Gating::Now(gated) => gated,
            Gating::Approving(_) => return Called::failed("delegated tool needs approval".into()),
        };
        match gated.gate {
            Gate::Run => {
                let called = registry.tools().run(tool, &gated.input, None, &child).await;
                Called {
                    outcome: registry
                        .hooks()
                        .post_tool(&child.point(), (tool, &gated.input), called.outcome)
                        .value,
                    card: called.card,
                }
            }
            Gate::Ask => Called::failed("delegated tool needs approval".into()),
            Gate::Refuse(why) => Called::failed(why),
        }
    }

    /// Changes what the model sees from here on: the log's entries
    /// `target..=upto` (or just `target`) stand as `replace`, text the
    /// model reads as a user's message, or are left out. The log itself is
    /// never rewritten; what compacts a transcript writes this.
    pub fn edit(&self, target: Seq, upto: Option<Seq>, replace: Option<&str>) {
        let replace = replace.and_then(|text| Json::encode(text).ok());
        self.host.edit(target, upto, replace);
    }

    /// Records a process group the call started, durably, so it is killed
    /// if the daemon dies mid-call; a `background` group the session owns
    /// past the call, until it ends.
    pub fn spawned(&self, pgid: u32, background: bool) {
        self.host.spawned(pgid, background);
    }

    /// Hands process group `pgid` to the session, past this call: it is
    /// recorded, and the session owns it until `ended`, which watches it,
    /// resolves.
    pub fn own(&self, pgid: u32, ended: impl Future<Output = ()> + Send + 'static) {
        self.host.own(pgid, Box::pin(ended));
    }

    /// Logs `text` as more of the call's output, for clients to show as
    /// it runs and for the call's outcome should it be interrupted.
    pub fn progress(&self, text: &str) {
        self.host.progress(text);
    }

    /// Side model requests on behalf of this call: see [`Models`](crate::Models).
    pub fn models(&self) -> Option<&dyn crate::Models> {
        self.service::<dyn crate::Models>()
    }

    /// Service `S`, if an extension or the engine provides it.
    pub fn service<S: ?Sized + 'static>(&self) -> Option<&S> {
        self.host
            .registry()
            .service::<S>()
            .or_else(|| self.services.get::<S>())
    }

    /// The call's session, as handlers at decision points see it.
    pub fn point(&self) -> PointCx<'_> {
        PointCx::new(self).about(self.call)
    }

    /// `spawn` as the `spawn` point leaves it: every process a tool starts
    /// goes through it, so a sandbox or a remote runner can wrap it; `Err`
    /// is why a handler refused it.
    pub fn spawn_hooks(&self, spawn: &mut SpawnSpec) -> Result<(), String> {
        (self.host.registry().hooks())
            .spawn(&self.point(), spawn)
            .map_err(|(name, why)| format!("{name}: {why}"))
    }

    /// The directory the session's commands run in: see [`CwdOf`].
    pub fn dir(&self) -> PathBuf {
        let mut dir = None;
        let registry = self.host.registry();
        self.host.states(&mut |states| {
            dir = Some(registry.dir(&self.workspace, self.env.as_deref(), states));
        });
        dir.unwrap_or_else(|| self.workspace.to_path_buf())
    }
}

impl View for Cx {
    fn session(&self) -> SessionId {
        self.session
    }

    fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn model(&self) -> String {
        self.model.to_string()
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        self.host.states(read);
    }

    fn env(&self) -> Option<Arc<Environment>> {
        self.env.clone()
    }

    fn cx(&self, call: CallId) -> Option<Cx> {
        (call == self.call).then(|| self.clone())
    }
}

/// A tool the model can call. Its input is read once, from the call's
/// text, into [`Tool::In`], whose type is its schema ([`Schema`]); its
/// output is the stored fact, encoded once. What the model sees of an
/// output is [`Tool::to_model`], and what clients show of a call the cards
/// [`Tool::present`] and [`Tool::present_result`] describe.
pub trait Tool: Send + Sync + 'static {
    type In: DeserializeOwned + Schema + Send + Sync;
    type Out: Serialize + Send;

    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    /// Who calls it: the model, or the user, as a command `/name args`
    /// given [`Args`].
    const AUDIENCE: Audience = Audience::Model;
    /// How a call left dangling by a crash is finished.
    const REPLAY: Replay = Replay::Unsafe;
    /// Whether its calls run alongside others of the turn.
    const CONCURRENCY: Concurrency = Concurrency::Exclusive;

    /// Runs a call. What it returns is its shape ([`Output`]): a
    /// `Result<Self::Out, String>` for a call done at once, a future of one
    /// for a call that waits (an `async fn`), or a [`Streamed`] call whose
    /// partial outputs are its progress.
    fn call(&self, input: &Self::In, cx: &Cx) -> impl Send;

    /// What the model sees of a stored output: by default the output as
    /// it was stored, its text sent as it is. A tool that tells the model
    /// something else reads the output it stored ([`Json::parse`]).
    fn to_model(output: &Json) -> llm::Output {
        llm::Output::Json(output.clone())
    }

    /// How clients show a call; pure, so a replay shows the same.
    fn present(_input: &Self::In) -> Option<Card> {
        None
    }

    /// How clients show a call's output; pure.
    fn present_result(_input: &Self::In, _output: &Self::Out) -> Option<Card> {
        None
    }
}

/// A call's input, read into its tool's own type: what its card is made
/// of and its run is given, so it is read once.
pub struct Parsed(Box<dyn Any + Send + Sync>);

impl Parsed {
    pub fn new<T: Any + Send + Sync>(input: T) -> Self {
        Self(Box::new(input))
    }

    pub fn get<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }

    fn take<T: Any>(self) -> Option<T> {
        self.0.downcast().ok().map(|input| *input)
    }
}

impl fmt::Debug for Parsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Parsed")
    }
}

/// How a call ended: its outcome, and how clients show it.
#[derive(Debug, PartialEq)]
pub struct Called {
    pub outcome: Outcome,
    pub card: Option<Card>,
}

impl Called {
    pub const fn failed(error: String) -> Self {
        Self {
            outcome: Outcome::Failed { error },
            card: None,
        }
    }
}

/// A tool over JSON text, so tools of every type, and tools named only at
/// run time, fit one toolset.
pub trait DynTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn audience(&self) -> Audience;
    fn replay(&self) -> Replay;
    fn concurrency(&self) -> Concurrency;
    /// How the tool is offered to the model, its schema written now.
    fn spec(&self) -> llm::Tool;
    /// Reads a call's input into the tool's own type.
    fn parse(&self, input: &Json) -> Result<Parsed, String>;
    /// Runs a call on an input [`DynTool::parse`] read.
    fn call<'a>(&'a self, input: Parsed, cx: &'a Cx) -> BoxFuture<'a, Called>;
    /// What the model sees of a stored output.
    fn to_model(&self, output: &Json) -> llm::Output;
    fn present(&self, input: &Parsed) -> Option<Card>;
}

/// A typed tool as a [`DynTool`], the shape `M` of its calls found from
/// what its `call` returns.
pub(crate) struct Erased<T, M>(pub T, pub PhantomData<fn() -> M>);

impl<T, M> DynTool for Erased<T, M>
where
    T: Tool,
    T::call(..): Output<T::Out, M>,
{
    fn name(&self) -> &str {
        T::NAME
    }

    fn description(&self) -> &str {
        T::DESCRIPTION
    }

    fn audience(&self) -> Audience {
        T::AUDIENCE
    }

    fn replay(&self) -> Replay {
        T::REPLAY
    }

    fn concurrency(&self) -> Concurrency {
        T::CONCURRENCY
    }

    fn spec(&self) -> llm::Tool {
        let schema = crate::schema::of::<T::In>();
        llm::Tool {
            name: llm::SmolStr::new_static(T::NAME),
            description: T::DESCRIPTION.to_owned(),
            schema: serde_json::from_str(&schema).expect("a derived schema is JSON"),
            strict: None,
        }
    }

    fn parse(&self, input: &Json) -> Result<Parsed, String> {
        (input.parse::<T::In>())
            .map(Parsed::new)
            .map_err(|e| format!("invalid input: {e}"))
    }

    fn call<'a>(&'a self, input: Parsed, cx: &'a Cx) -> BoxFuture<'a, Called> {
        let Some(input) = input.take::<T::In>() else {
            let error = format!("the input is not one {} reads", T::NAME);
            return Box::pin(std::future::ready(Called::failed(error)));
        };
        Box::pin(async move {
            let progress = |text: &str| cx.progress(text);
            let output = match self.0.call(&input, cx).run(&progress).await {
                Ok(output) => output,
                Err(error) => return Called::failed(error),
            };
            match Json::encode(&output) {
                Ok(json) => Called {
                    card: T::present_result(&input, &output),
                    outcome: Outcome::Done { output: json },
                },
                Err(e) => Called::failed(e.to_string()),
            }
        })
    }

    fn to_model(&self, output: &Json) -> llm::Output {
        T::to_model(output)
    }

    fn present(&self, input: &Parsed) -> Option<Card> {
        T::present(input.get()?)
    }
}
