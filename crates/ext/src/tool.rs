//! Typed tools, the erased form a toolset holds, and what a call runs
//! with.

use std::any::TypeId;
use std::collections::BTreeMap;
use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Poll, Waker};

use protocol::{Audience, Card, Environment, Json, Outcome, SessionId};
use serde::Serialize;
use serde::de::{Deserialize, DeserializeOwned};
use serde_json::Value;

use crate::BoxFuture;
#[cfg(doc)]
use crate::CwdOf;
use crate::host::Host;
use crate::point::{PointCx, View};
use crate::state::{self, Fold, States};

/// Whether a call interrupted by a crash may run again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Replay {
    Safe,
    #[default]
    Unsafe,
}

/// What a user's command is given: the text after its name.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Args {
    #[serde(default)]
    pub args: String,
}

/// What the kernel knows of a callable: who may call it, and whether a
/// call a crash cut off runs again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Callable {
    pub audience: Audience,
    pub replay: Replay,
}

/// Where a call runs: tools run in the daemon, and later ones may run in
/// a client, routed to the clients it names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Route {
    #[default]
    Local,
    /// The client that owns the session.
    Owner,
    /// The client that sent the last input.
    LastActive,
    /// Every client declaring the tool; the first answer wins.
    Broadcast,
}

/// Set when a running turn should stop: the user interrupted it, or the
/// session or daemon is going away. Tools that block poll it; async work
/// awaits [`Cancel::cancelled`].
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<Flag>);

#[derive(Debug, Default)]
struct Flag {
    set: AtomicBool,
    /// The tasks awaiting it.
    waiting: Mutex<Vec<Waker>>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.0.set.store(true, Ordering::SeqCst);
        let waiting = std::mem::take(&mut *self.waiting());
        waiting.into_iter().for_each(Waker::wake);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.set.load(Ordering::SeqCst)
    }

    /// Resolves once cancelled.
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + '_ {
        poll_fn(|cx| {
            if self.is_cancelled() {
                return Poll::Ready(());
            }
            let mut waiting = self.waiting();
            // Checked again under the lock `cancel` takes to wake.
            if self.is_cancelled() {
                return Poll::Ready(());
            }
            if !waiting.iter().any(|w| w.will_wake(cx.waker())) {
                waiting.push(cx.waker().clone());
            }
            Poll::Pending
        })
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, Vec<Waker>> {
        self.0
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
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
    /// The call's id: the seq of its intent.
    pub call: u64,
    /// The model the session talks to, as `host:model`.
    pub model: Arc<str>,
    /// The session's directory for output too large for the log, created
    /// on first use.
    pub spill: Arc<Path>,
    pub cancel: Cancel,
    host: Arc<dyn Host>,
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
        } = call;
        Self {
            workspace,
            env,
            session,
            call,
            model,
            spill,
            cancel,
            host,
        }
    }

    /// Reads state `S`, or its default when no extension registered it.
    pub fn state<S: Fold, R>(&self, read: impl FnOnce(&S) -> R) -> R {
        state::read(|f| self.host.states(f), read)
    }

    /// Commits `change` to state `S`; nothing when no extension registered
    /// it.
    pub fn update<S: Fold>(&self, change: S::Change) {
        let mut ext = None;
        self.host
            .states(&mut |states| ext = states.ext_of::<S>().cloned());
        let Some(ext) = ext else { return };
        match serde_json::to_value(change) {
            Ok(data) => self.host.note(&ext, S::NAME, data),
            Err(e) => eprintln!("warning: extension {ext}: a {} change: {e}", S::NAME),
        }
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

    /// Commits `text` as more of the call's output, for clients to show as
    /// it runs and for the call's outcome should it be interrupted.
    pub fn progress(&self, text: &str) {
        self.host.progress(text);
    }

    /// Service `S`, if an extension or the engine provides it.
    pub fn service<S: ?Sized + 'static>(&self) -> Option<&S> {
        self.host.registry().service::<S>().or_else(|| {
            let service = self.host.service(TypeId::of::<S>())?;
            service.downcast_ref::<Arc<S>>().map(|s| &**s)
        })
    }

    /// The call's session, as handlers at decision points see it.
    pub fn point(&self) -> PointCx<'_> {
        PointCx::new(self.session, &self.workspace, self)
    }

    /// `spawn` as the `spawn` point leaves it: every process a tool starts
    /// goes through it, so a sandbox or a remote runner can wrap it.
    pub fn spawn_hooks(&self, spawn: &mut SpawnSpec) {
        self.host.registry().hooks().spawn(&self.point(), spawn);
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
    fn model(&self) -> String {
        self.model.to_string()
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        self.host.states(read);
    }
}

/// A tool the model can call. Its output is the stored fact; what the model
/// sees of it is [`Tool::to_model`], and what clients show of it the cards
/// [`Tool::present`] and [`Tool::present_result`] describe.
pub trait Tool: Send + Sync + 'static {
    type In: DeserializeOwned + Send;
    type Out: Serialize + DeserializeOwned;

    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    /// Who calls it: the model, or the user, as a command `/name args`
    /// given [`Args`].
    const AUDIENCE: Audience = Audience::Model;
    /// How a call left dangling by a crash is finished.
    const REPLAY: Replay = Replay::Unsafe;
    const ROUTE: Route = Route::Local;

    /// The JSON Schema of [`Tool::In`].
    fn schema() -> Value;

    fn call(
        &self,
        input: Self::In,
        cx: &Cx,
    ) -> impl Future<Output = Result<Self::Out, String>> + Send;

    fn to_model(output: &Self::Out) -> llm::Output {
        llm::Output::Json(serde_json::to_value(output).unwrap_or_default())
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

/// A tool over JSON, so tools of every type, and tools named only at run
/// time, fit one toolset.
pub trait DynTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn audience(&self) -> Audience;
    fn replay(&self) -> Replay;
    fn route(&self) -> Route;
    /// How the tool is offered to the model.
    fn spec(&self) -> llm::Tool;
    fn call<'a>(&'a self, input: &Value, cx: &'a Cx) -> BoxFuture<'a, Outcome>;
    /// What the model sees of a stored output.
    fn to_model(&self, output: &Json) -> llm::Output;
    fn present(&self, input: &Value) -> Option<Card>;
    fn present_result(&self, input: &Value, output: &Json) -> Option<Card>;
}

impl<T: Tool> DynTool for T {
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

    fn route(&self) -> Route {
        T::ROUTE
    }

    fn spec(&self) -> llm::Tool {
        llm::Tool {
            name: llm::SmolStr::new_static(T::NAME),
            description: T::DESCRIPTION.to_owned(),
            schema: T::schema(),
            strict: None,
        }
    }

    fn call<'a>(&'a self, input: &Value, cx: &'a Cx) -> BoxFuture<'a, Outcome> {
        let input = T::In::deserialize(input).map_err(|e| format!("invalid input: {e}"));
        Box::pin(async move {
            let result = match input {
                Ok(input) => Tool::call(self, input, cx).await,
                Err(error) => Err(error),
            }
            .and_then(|output| serde_json::to_value(output).map_err(|e| e.to_string()));
            match result {
                Ok(output) => Outcome::Done {
                    output: output.into(),
                },
                Err(error) => Outcome::Failed { error },
            }
        })
    }

    fn to_model(&self, output: &Json) -> llm::Output {
        output
            .parse::<T::Out>()
            .map_or_else(|_| llm::Output::Json(output.value()), |o| T::to_model(&o))
    }

    fn present(&self, input: &Value) -> Option<Card> {
        T::present(&T::In::deserialize(input).ok()?)
    }

    fn present_result(&self, input: &Value, output: &Json) -> Option<Card> {
        let input = T::In::deserialize(input).ok()?;
        T::present_result(&input, &output.parse().ok()?)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Wake, Waker};

    use super::Cancel;

    #[derive(Default)]
    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn cancelled_wakes_whoever_waits() {
        let cancel = Cancel::default();
        let count = Arc::new(Count::default());
        let waker = Waker::from(Arc::clone(&count));
        let mut cx = Context::from_waker(&waker);
        let mut waiting = pin!(cancel.cancelled());
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        assert!(waiting.as_mut().poll(&mut cx).is_pending());
        cancel.clone().cancel();
        assert!(count.0.load(Ordering::SeqCst) > 0);
        assert!(waiting.as_mut().poll(&mut cx).is_ready());
        assert!(pin!(cancel.cancelled()).poll(&mut cx).is_ready());
    }
}
