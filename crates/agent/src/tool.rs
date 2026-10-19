//! Typed tools, the erased form a [`Toolset`] holds, and what a call runs
//! with.

use std::future::{Future, poll_fn};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use protocol::{Card, Environment, Json, Outcome, SessionId};
use rt::BoxFuture;
use serde::Serialize;
use serde::de::{Deserialize, DeserializeOwned};
use serde_json::Value;
use sys::Signal;

use crate::state::{Cwd, State, States};
use crate::{Hooks, Spawn};

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
    wakers: Mutex<Vec<Waker>>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.0.set.store(true, Ordering::SeqCst);
        let wakers = std::mem::take(&mut *self.wakers());
        wakers.into_iter().for_each(Waker::wake);
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
            let mut wakers = self.wakers();
            if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
            drop(wakers);
            // Cancelled between the check and the registration: its wake
            // may have missed this waker.
            if self.is_cancelled() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }

    fn wakers(&self) -> MutexGuard<'_, Vec<Waker>> {
        self.0.wakers.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// Moves the stop along; call it until the leader has exited.
    pub fn tick(&mut self) {
        match self.asked {
            None => {
                self.asked = Some(Instant::now());
                let _ = sys::signal_group(self.pgid, Signal::Terminate);
            }
            Some(at) if at.elapsed() >= Self::GRACE => self.now(),
            Some(_) => {}
        }
    }

    /// SIGKILL, at once.
    pub fn now(&mut self) {
        let _ = sys::signal_group(self.pgid, Signal::Kill);
    }
}

/// The engine, as a running call reaches it.
pub trait Host: Send + Sync {
    /// Records a process group the call started, durably, so it is killed
    /// if the daemon dies mid-call; a `background` group the session owns
    /// past the call, until it ends.
    fn spawned(&self, pgid: u32, background: bool);
    /// Commits `text` as more of the call's output, for clients to show as
    /// it runs and for the call's outcome should it be interrupted.
    fn progress(&self, text: &str);
    /// Commits `data` as a change to extension `ext`'s state `state`, and
    /// folds it in.
    fn note(&self, ext: &'static str, state: &'static str, data: Value);
    fn states(&self) -> MutexGuard<'_, States>;
    fn hooks(&self) -> &Hooks;
}

/// What a tool or command call runs with. Cheap to clone, and owned, so
/// work the call hands off (to a blocking thread, a background task) can
/// take it along.
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
    pub host: Arc<dyn Host>,
}

impl Cx {
    /// Reads state `S`, or its default when no extension registered it.
    pub fn state<S: State, R>(&self, read: impl FnOnce(&S) -> R) -> R {
        let states = self.host.states();
        match states.get::<S>() {
            Some(state) => read(state),
            None => read(&S::default()),
        }
    }

    /// Commits `data` as a change to state `S`; nothing when no extension
    /// registered it.
    pub fn update<S: State>(&self, data: Value) {
        let ext = self.host.states().ext_of::<S>();
        if let Some(ext) = ext {
            self.host.note(ext, S::NAME, data);
        }
    }

    pub fn spawned(&self, pgid: u32, background: bool) {
        self.host.spawned(pgid, background);
    }

    /// See [`Host::progress`].
    pub fn progress(&self, text: &str) {
        self.host.progress(text);
    }

    /// `spawn` as the `spawn` hooks leave it: every process a tool starts
    /// goes through them, so a sandbox or a remote runner can wrap it.
    pub fn spawn_hooks(&self, spawn: &mut Spawn) {
        self.host.hooks().spawn(spawn);
    }

    /// The directory the session's commands run in: see [`session_dir`].
    pub fn dir(&self) -> PathBuf {
        session_dir(&self.workspace, self.env.as_deref(), &self.host.states())
    }
}

/// The directory a session's commands run in: where its last command left
/// off, unless the client has since moved, else the client's directory,
/// else the workspace. One that was removed since is skipped.
pub fn session_dir(workspace: &Path, env: Option<&Environment>, states: &States) -> PathBuf {
    let base = env.map(|env| env.cwd.as_path());
    let last = states.get::<Cwd>().and_then(|cwd| cwd.after(base));
    last.into_iter()
        .chain(base)
        .find(|dir| dir.is_dir())
        .unwrap_or(workspace)
        .to_path_buf()
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

/// A [`Tool`] over JSON, so tools of every type fit one [`Toolset`].
pub trait DynTool: Send + Sync {
    fn name(&self) -> &'static str;
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
    fn name(&self) -> &'static str {
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

/// The tools an agent may call, by name, as a [`Registry`](crate::Registry)
/// resolves them.
#[derive(Default)]
pub struct Toolset(pub(crate) Vec<Arc<dyn DynTool>>);

impl Toolset {
    pub fn get(&self, name: &str) -> Option<&dyn DynTool> {
        self.0.iter().map(|t| &**t).find(|t| t.name() == name)
    }

    /// Every tool, as offered to the model.
    pub fn specs(&self) -> Vec<llm::Tool> {
        self.0.iter().map(|t| t.spec()).collect()
    }

    pub fn run<'a>(&'a self, name: &str, input: &Value, cx: &'a Cx) -> BoxFuture<'a, Outcome> {
        match self.get(name) {
            Some(tool) => tool.call(input, cx),
            None => Box::pin(std::future::ready(Outcome::Failed {
                error: format!("unknown tool {name}"),
            })),
        }
    }

    /// What the model sees of a call's outcome.
    pub fn to_model(&self, name: &str, outcome: &Outcome) -> (llm::Output, bool) {
        match outcome {
            Outcome::Done { output } => (
                self.get(name)
                    .map_or_else(|| llm::Output::Json(output.value()), |t| t.to_model(output)),
                false,
            ),
            Outcome::Failed { error } => (llm::Output::Text(error.clone()), true),
            Outcome::Interrupted { output } => {
                let mut text = "interrupted before it finished".to_owned();
                if let Some(output) = output.as_deref().filter(|o| !o.is_empty()) {
                    text = format!("{output}\n[{text}]");
                }
                (llm::Output::Text(text), true)
            }
        }
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
