//! Typed tools, the erased form a [`Toolset`] holds, and what a call runs
//! with.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, MutexGuard};
use std::time::{Duration, Instant};

use protocol::{Card, Environment, Outcome, SessionId};
use rt::{BoxFuture, Runtime};
use serde::Serialize;
use serde::de::{Deserialize, DeserializeOwned};
use serde_json::Value;
use sys::Signal;

use crate::state::{Cwd, State, States};
use crate::{Hooks, Spawn};

/// Whether a call interrupted by a crash may simply run again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replay {
    Safe,
    Unsafe,
}

/// Set when a running call should stop: the user interrupted it, or the
/// session or daemon is going away. Tools that block poll it.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
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
pub trait Host: Sync {
    /// Records a process group the call started, durably, so it is killed
    /// if the daemon dies mid-call; a `background` group the session owns
    /// past the call, until it ends.
    fn spawned(&self, pgid: u32, background: bool);
    /// Commits `data` as a change to extension `ext`'s state `state`, and
    /// folds it in.
    fn note(&self, ext: &'static str, state: &'static str, data: Value);
    fn states(&self) -> MutexGuard<'_, States>;
    fn hooks(&self) -> &Hooks;
}

/// What a tool or command call runs with.
pub struct Cx<'a> {
    /// The session's workspace.
    pub workspace: &'a Path,
    /// The attaching client's environment, which processes run in. `None`
    /// (a session no client has described) inherits the daemon's.
    pub env: Option<&'a Environment>,
    pub session: SessionId,
    /// The call's id: the seq of its intent.
    pub call: u64,
    /// The model the session talks to, as `host:model`.
    pub model: &'a str,
    /// The session's directory for output too large for the log, created
    /// on first use.
    pub spill: &'a Path,
    pub cancel: &'a Cancel,
    /// Where a tool sleeps and runs blocking work, so it never holds up a
    /// runtime worker.
    pub rt: &'a dyn Runtime,
    pub host: &'a dyn Host,
}

impl Cx<'_> {
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

    /// `spawn` as the `spawn` hooks leave it: every process a tool starts
    /// goes through them, so a sandbox or a remote runner can wrap it.
    pub fn spawn_hooks(&self, spawn: &mut Spawn) {
        self.host.hooks().spawn(spawn);
    }

    /// The directory the session's commands run in: see [`session_dir`].
    pub fn dir(&self) -> PathBuf {
        session_dir(self.workspace, self.env, &self.host.states())
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

    /// The JSON Schema of [`Tool::In`].
    fn schema() -> Value;

    fn call(
        &self,
        input: Self::In,
        cx: &Cx<'_>,
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
    /// How the tool is offered to the model.
    fn spec(&self) -> llm::Tool;
    fn call<'a>(&'a self, input: &Value, cx: &'a Cx<'a>) -> BoxFuture<'a, Outcome>;
    /// What the model sees of a stored output.
    fn to_model(&self, output: &Value) -> llm::Output;
    fn present(&self, input: &Value) -> Option<Card>;
    fn present_result(&self, input: &Value, output: &Value) -> Option<Card>;
}

impl<T: Tool> DynTool for T {
    fn name(&self) -> &'static str {
        T::NAME
    }

    fn replay(&self) -> Replay {
        T::REPLAY
    }

    fn spec(&self) -> llm::Tool {
        llm::Tool {
            name: llm::SmolStr::new_static(T::NAME),
            description: T::DESCRIPTION.to_owned(),
            schema: T::schema(),
            strict: None,
        }
    }

    fn call<'a>(&'a self, input: &Value, cx: &'a Cx<'a>) -> BoxFuture<'a, Outcome> {
        let input = T::In::deserialize(input).map_err(|e| format!("invalid input: {e}"));
        Box::pin(async move {
            let result = match input {
                Ok(input) => Tool::call(self, input, cx).await,
                Err(error) => Err(error),
            }
            .and_then(|output| serde_json::to_value(output).map_err(|e| e.to_string()));
            match result {
                Ok(output) => Outcome::Done { output },
                Err(error) => Outcome::Failed { error },
            }
        })
    }

    fn to_model(&self, output: &Value) -> llm::Output {
        T::Out::deserialize(output)
            .map_or_else(|_| llm::Output::Json(output.clone()), |o| T::to_model(&o))
    }

    fn present(&self, input: &Value) -> Option<Card> {
        T::present(&T::In::deserialize(input).ok()?)
    }

    fn present_result(&self, input: &Value, output: &Value) -> Option<Card> {
        let input = T::In::deserialize(input).ok()?;
        T::present_result(&input, &T::Out::deserialize(output).ok()?)
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

    pub fn run<'a>(&'a self, name: &str, input: &Value, cx: &'a Cx<'a>) -> BoxFuture<'a, Outcome> {
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
                    .map_or_else(|| llm::Output::Json(output.clone()), |t| t.to_model(output)),
                false,
            ),
            Outcome::Failed { error } => (llm::Output::Text(error.clone()), true),
            Outcome::Interrupted => (
                llm::Output::Text("interrupted: the engine stopped mid-call".to_owned()),
                true,
            ),
        }
    }
}
