//! Typed tools, the erased form a toolset holds, and what a call runs
//! with.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use protocol::{Card, Environment, Json, Outcome, SessionId};
use rt::BoxFuture;
use serde::Serialize;
use serde::de::{Deserialize, DeserializeOwned};
use serde_json::Value;
use sys::Signal;

#[cfg(doc)]
use crate::CwdOf;
use crate::host::Host;
use crate::job;
use crate::point::{PointCx, View};
use crate::state::{self, Fold, States};

/// Whether a call interrupted by a crash may run again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    Safe,
    Unsafe,
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
    notify: rt::Notify,
}

impl Cancel {
    pub fn cancel(&self) {
        self.0.set.store(true, Ordering::SeqCst);
        self.0.notify.notify();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.set.load(Ordering::SeqCst)
    }

    /// Resolves once cancelled.
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + '_ {
        self.0.notify.until(|| self.is_cancelled())
    }
}

/// Stops a process group the way every tool should: SIGTERM, then SIGKILL
/// once [`Kill::GRACE`] has passed without the group's leader exiting.
pub struct Kill {
    pgid: u32,
    asked: Option<Instant>,
}

impl Kill {
    pub const GRACE: Duration = Duration::from_secs(3);

    pub fn new(pgid: u32) -> Self {
        Self { pgid, asked: None }
    }

    /// Moves the stop along; call it again by the time it returns, until
    /// the leader has exited. `None` once it is down to SIGKILL.
    pub fn tick(&mut self) -> Option<Instant> {
        match self.asked {
            None => {
                let now = Instant::now();
                self.asked = Some(now);
                let _ = sys::signal_group(self.pgid, Signal::Terminate);
                Some(now + Self::GRACE)
            }
            Some(at) if at.elapsed() >= Self::GRACE => {
                self.now();
                None
            }
            Some(at) => Some(at + Self::GRACE),
        }
    }

    /// SIGKILL, at once.
    pub fn now(&mut self) {
        let _ = sys::signal_group(self.pgid, Signal::Kill);
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
    /// Where a tool sleeps and runs blocking work, so it never holds up a
    /// runtime worker.
    pub rt: rt::Rt,
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
            rt,
        } = call;
        Self {
            workspace,
            env,
            session,
            call,
            model,
            spill,
            cancel,
            rt,
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
    /// recorded, then watched until it ends (reaping `leader`, the child
    /// that leads it, when it exits), when [`job::ENDED`] is notified.
    pub fn own(&self, pgid: u32, leader: Option<Child>) {
        self.host.spawned(pgid, true);
        self.rt.spawn(Box::pin(async move {
            job::ended(pgid, leader).await;
            job::ENDED.notify();
        }));
    }

    /// Commits `text` as more of the call's output, for clients to show as
    /// it runs and for the call's outcome should it be interrupted.
    pub fn progress(&self, text: &str) {
        self.host.progress(text);
    }

    /// Service `S`, if an extension provides it.
    pub fn service<S: ?Sized + 'static>(&self) -> Option<&S> {
        self.host.registry().service::<S>()
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
    use std::time::Duration;

    use super::Cancel;

    #[test]
    fn cancelled_wakes_whoever_waits() {
        let cancel = Cancel::default();
        let other = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            other.cancel();
        });
        rt::block_on(cancel.cancelled());
        canceller.join().unwrap();
        assert!(cancel.is_cancelled());
        rt::block_on(cancel.cancelled());
    }
}
