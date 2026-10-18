use std::collections::{HashSet, VecDeque};
use std::fmt::Display;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use agent::{Action, Agent, Cx, Decided, DynTool, Hooks, Input, Replay, Toolset};
use protocol::{Body, DaemonMsg, Entry, Environment, Outcome, SessionId, SessionSummary};
use serde_json::Value;
use store::{Header, Loaded, Log, LogStore};

use crate::lock;
use crate::model::Model;

mod facts;
#[cfg(test)]
mod tests;

use facts::{replay, transcript};

/// What every session runs with.
pub struct Shared {
    pub store: Arc<dyn LogStore>,
    pub agent: agent::Kind,
    pub tools: Toolset,
    pub hooks: Hooks,
    /// The model, or why there is none (shown as the failed model call).
    pub model: Result<Model, String>,
}

pub struct Session {
    pub id: SessionId,
    pub workspace: PathBuf,
    state: Mutex<State>,
    inbox: Sender<Msg>,
    /// Inputs submitted but not yet fully handled. They pin the daemon and
    /// keep the session loaded.
    queued: AtomicUsize,
    /// Open client attachments; a session with any stays loaded.
    attached: AtomicUsize,
    last_active: Mutex<Instant>,
    /// What commands run in: the last client's capture, secrets included,
    /// or after a restart the last one logged, without them.
    env: Mutex<Option<Environment>>,
    shared: Arc<Shared>,
}

enum Msg {
    Input(String),
    Stop,
}

struct State {
    entries: Vec<Entry>,
    log: Box<dyn Log>,
    clients: Vec<Sender<DaemonMsg>>,
}

/// A client's hold on a session; dropping it detaches.
pub struct Attachment(Arc<Session>);

impl Attachment {
    pub fn session(&self) -> &Session {
        &self.0
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.0.attached.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

impl Session {
    /// Starts a loaded session's actor: the single writer that steps the
    /// agent and runs its effects. Calls left dangling by a crash are
    /// recovered first.
    pub fn start(header: Header, loaded: Loaded, shared: Arc<Shared>) -> Arc<Self> {
        let (inbox, rx) = mpsc::channel();
        let env = logged_env(&loaded.entries).cloned();
        let session = Arc::new(Self {
            id: header.id,
            workspace: header.workspace,
            state: Mutex::new(State {
                entries: loaded.entries,
                log: loaded.log,
                clients: Vec::new(),
            }),
            inbox,
            // Recovery counts as queued work until it has run.
            queued: AtomicUsize::new(1),
            attached: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            env: Mutex::new(env),
            shared,
        });
        let actor = Arc::clone(&session);
        thread::spawn(move || actor.run(&rx));
        session
    }

    /// Opens a client attachment. Callers hold the session's slot lock, so a
    /// session cannot be unloaded between being found and being attached.
    pub fn open(self: &Arc<Self>) -> Attachment {
        self.attached.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Attachment(Arc::clone(self))
    }

    pub fn submit(&self, text: String) {
        self.queued.fetch_add(1, Ordering::SeqCst);
        let _ = self.inbox.send(Msg::Input(text));
    }

    pub fn has_work(&self) -> bool {
        self.queued.load(Ordering::SeqCst) > 0
    }

    /// How long the session has had no attachments and no work.
    pub fn idle_for(&self) -> Duration {
        if self.attached.load(Ordering::SeqCst) > 0 || self.has_work() {
            return Duration::ZERO;
        }
        lock(&self.last_active).elapsed()
    }

    /// Stops the actor; the session lives on in its log. Callers check
    /// `idle_for` under the slot lock first.
    pub fn stop(&self) {
        let _ = self.inbox.send(Msg::Stop);
    }

    pub fn attach(&self, client: &Sender<DaemonMsg>, since: u64) {
        let mut st = self.lock();
        let _ = client.send(DaemonMsg::Attached {
            session: self.id,
            workspace: self.workspace.clone(),
        });
        for e in st.entries.iter().filter(|e| e.seq > since) {
            let _ = client.send(DaemonMsg::Entry(e.clone()));
        }
        let seq = st.entries.last().map_or(0, |e| e.seq);
        let _ = client.send(DaemonMsg::Synced { seq });
        st.clients.push(client.clone());
    }

    /// Runs commands from now on in `env`, the newest client's. Its redacted
    /// form is logged when that differs from the last one logged, so
    /// re-attaching from the same shell adds nothing.
    pub fn set_env(&self, env: Environment) -> io::Result<()> {
        let logged = env.redacted();
        *lock(&self.env) = Some(env);
        if logged_env(&self.lock().entries) == Some(&logged) {
            return Ok(());
        }
        self.append(Body::Environment(logged)).map(drop)
    }

    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id,
            workspace: self.workspace.clone(),
            entries: self.lock().entries.len() as u64,
            clients: u32::try_from(self.attached.load(Ordering::SeqCst)).unwrap_or(u32::MAX),
        }
    }

    fn touch(&self) {
        *lock(&self.last_active) = Instant::now();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn warn(&self, problem: impl Display) {
        eprintln!("session {}: {problem}", self.id.short());
    }

    /// Commits an entry; only committed entries are ever shown to clients.
    fn append(&self, body: Body) -> io::Result<u64> {
        let mut st = self.lock();
        let seq = st.entries.len() as u64 + 1;
        let entry = Entry {
            seq,
            epoch: 0,
            body,
        };
        st.log.append(&entry)?;
        st.clients
            .retain(|c| c.send(DaemonMsg::Entry(entry.clone())).is_ok());
        self.shared.hooks.observe(&entry);
        st.entries.push(entry);
        Ok(seq)
    }

    /// Commits what the hooks decided.
    fn decide(&self, decided: Decided) -> io::Result<()> {
        for (hook, decision) in decided {
            self.append(Body::Hook {
                hook: hook.to_owned(),
                decision,
            })?;
        }
        Ok(())
    }

    /// Sends a message that is not part of the log to every attached client.
    fn broadcast(&self, msg: &DaemonMsg) {
        self.lock().clients.retain(|c| c.send(msg.clone()).is_ok());
    }

    fn run(&self, inbox: &Receiver<Msg>) {
        let mut agent = self.shared.agent.agent();
        self.work(|| self.recover(agent.as_mut()));
        for msg in inbox {
            let Msg::Input(text) = msg else { return };
            self.work(|| {
                self.append(Body::User { text: text.clone() })?;
                let inputs = VecDeque::from([Input::User { text }]);
                self.drive(agent.as_mut(), VecDeque::new(), inputs)
            });
        }
    }

    /// Runs one queued unit of work, with a busy marker on disk for as long
    /// as anything is in flight.
    fn work(&self, job: impl FnOnce() -> io::Result<()>) {
        let store = &self.shared.store;
        if let Err(e) = store.mark_busy(self.id, true) {
            self.warn(format_args!("busy marker: {e}"));
        }
        if let Err(e) = job() {
            self.warn(e);
        }
        if self.queued.fetch_sub(1, Ordering::SeqCst) == 1
            && let Err(e) = store.mark_busy(self.id, false)
        {
            self.warn(format_args!("busy marker: {e}"));
        }
        self.touch();
    }

    /// Runs `actions`, then steps the agent with `inputs` and every input
    /// the effects produce, in order. Once an agent that was stepped stops,
    /// the `on_stop` hooks may hand it more to do.
    fn drive(
        &self,
        agent: &mut dyn Agent,
        mut actions: VecDeque<Action>,
        mut inputs: VecDeque<Input>,
    ) -> io::Result<()> {
        let mut stepped = false;
        loop {
            while let Some(action) = actions.pop_front() {
                inputs.extend(self.act(action)?);
            }
            if let Some(input) = inputs.pop_front() {
                actions.extend(agent.step(input));
                stepped = true;
                continue;
            }
            if !std::mem::take(&mut stepped) {
                return Ok(());
            }
            let mut decided = Decided::new();
            let next = self
                .shared
                .hooks
                .on_stop(&self.lock().entries, &mut decided);
            self.decide(decided)?;
            match next {
                Some(text) => inputs.push_back(Input::User { text }),
                None => return Ok(()),
            }
        }
    }

    /// Carries out one action; an effect returns the input reporting its
    /// outcome.
    fn act(&self, action: Action) -> io::Result<Option<Input>> {
        let (call, seq, outcome) = match action {
            Action::Emit(body) => {
                self.append(body)?;
                return Ok(None);
            }
            Action::CallModel => {
                let model = self
                    .shared
                    .model
                    .as_ref()
                    .map_or_else(|_| "none".to_owned(), Model::name);
                let seq = self.append(Body::ModelCall { model })?;
                (Call::Model, seq, self.run_model(seq))
            }
            Action::RunTool {
                tool,
                mut input,
                id,
                refuse,
            } => {
                let verdict = if let Some(error) = refuse {
                    Err(error)
                } else {
                    let mut decided = Decided::new();
                    let verdict = self.shared.hooks.gate(&tool, &mut input, &mut decided);
                    self.decide(decided)?;
                    verdict
                };
                let seq = self.append(Body::ToolCall {
                    tool: tool.clone(),
                    input: input.clone(),
                    id,
                })?;
                let outcome = match verdict {
                    Ok(()) => self.run_tool(seq, &tool, &input),
                    Err(error) => Outcome::Failed { error },
                };
                (Call::Tool { tool, input }, seq, outcome)
            }
        };
        self.finish(&call, seq, outcome).map(Some)
    }

    fn run_call(&self, call: &Call, seq: u64) -> Outcome {
        match call {
            Call::Model => self.run_model(seq),
            Call::Tool { tool, input } => self.run_tool(seq, tool, input),
        }
    }

    /// Commits the outcome of call `seq`, as the `post_tool` hooks leave a
    /// tool's; returns the input that reports it to the agent.
    fn finish(&self, call: &Call, seq: u64, mut outcome: Outcome) -> io::Result<Input> {
        let (body, input) = match call {
            Call::Model => (
                Body::ModelResult {
                    call: seq,
                    outcome: outcome.clone(),
                },
                Input::ModelDone { call: seq, outcome },
            ),
            Call::Tool { tool, input } => {
                let mut decided = Decided::new();
                let hooks = &self.shared.hooks;
                hooks.post_tool(seq, (tool, input), &mut outcome, &mut decided);
                self.decide(decided)?;
                (
                    Body::ToolResult {
                        call: seq,
                        outcome: outcome.clone(),
                    },
                    Input::ToolDone { call: seq, outcome },
                )
            }
        };
        self.append(body)?;
        Ok(input)
    }

    fn run_tool(&self, call: u64, tool: &str, input: &Value) -> Outcome {
        let spawned = |pgid| {
            let Some(boot) = sys::boot_id() else { return };
            let body = Body::ToolProcess {
                call,
                pgid,
                boot: boot.to_owned(),
            };
            if let Err(e) = self.append(body) {
                self.warn(format_args!("recording process {pgid}: {e}"));
            }
        };
        let env = lock(&self.env).clone();
        let cx = Cx {
            cwd: &self.workspace,
            env: env.as_ref(),
            spawned: &spawned,
        };
        contain(|| self.shared.tools.run(tool, input, &cx))
    }

    /// Asks the model to reply to the transcript before `call`, offering the
    /// tools. Text streams to clients as it arrives; only the finished reply
    /// is committed.
    fn run_model(&self, call: u64) -> Outcome {
        let model = match &self.shared.model {
            Ok(model) => model,
            Err(error) => {
                return Outcome::Failed {
                    error: error.clone(),
                };
            }
        };
        let tools = &self.shared.tools;
        let history = transcript(&self.lock().entries, call, tools);
        let mut request = model.request(history, tools.specs());
        let mut decided = Decided::new();
        self.shared
            .hooks
            .prepare_step(call, &mut request, &mut decided);
        if let Err(e) = self.decide(decided) {
            return Outcome::Failed {
                error: e.to_string(),
            };
        }
        let mut on_text = |text: &str| {
            self.broadcast(&DaemonMsg::Delta {
                call,
                text: text.to_owned(),
            });
        };
        // The whole reply is the fact: replaying it later sends the
        // provider's own items (signatures, encrypted reasoning) back.
        contain(|| match model.reply(&request, &mut on_text) {
            Ok(reply) => match serde_json::to_value(&reply) {
                Ok(output) => Outcome::Done { output },
                Err(error) => Outcome::Failed {
                    error: error.to_string(),
                },
            },
            Err(error) => Outcome::Failed {
                error: error.to_string(),
            },
        })
    }

    /// Picks up where the log leaves off. The agent is rebuilt from the
    /// log; calls with an intent but no outcome are finished (leftover
    /// processes killed, safe calls run again, the rest marked interrupted);
    /// then whatever the agent asked for that the log does not show runs.
    fn recover(&self, agent: &mut dyn Agent) -> io::Result<()> {
        let (dangling, actions) = {
            let st = self.lock();
            (dangling_calls(&st.entries), replay(agent, &st.entries))
        };
        let mut inputs = VecDeque::new();
        for Dangling {
            seq,
            call,
            processes,
        } in dangling
        {
            let this_boot = processes
                .iter()
                .filter(|(_, boot)| Some(boot.as_str()) == sys::boot_id());
            for (pgid, _) in this_boot {
                if let Err(e) = sys::kill_process_group(*pgid) {
                    self.warn(format_args!("killing process group {pgid}: {e}"));
                }
            }
            let outcome = match &call {
                Call::Tool { tool, .. }
                    if self.shared.tools.get(tool).map(DynTool::replay) != Some(Replay::Safe) =>
                {
                    Outcome::Interrupted
                }
                _ => self.run_call(&call, seq),
            };
            inputs.push_back(self.finish(&call, seq, outcome)?);
        }
        self.drive(agent, actions, inputs)
    }
}

/// The environment most recently logged.
fn logged_env(entries: &[Entry]) -> Option<&Environment> {
    entries.iter().rev().find_map(|e| match &e.body {
        Body::Environment(env) => Some(env),
        _ => None,
    })
}

/// Runs an effect so that a panic inside it fails the call instead of taking
/// the session actor, and with it the daemon, down.
fn contain(effect: impl FnOnce() -> Outcome) -> Outcome {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(effect)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("unknown panic");
        Outcome::Failed {
            error: format!("panicked: {message}"),
        }
    })
}

/// The effect an agent asked for, between its committed intent and outcome.
enum Call {
    Model,
    Tool { tool: String, input: Value },
}

/// A call with an intent but no outcome, and the process groups it started.
struct Dangling {
    seq: u64,
    call: Call,
    processes: Vec<(u32, String)>,
}

fn dangling_calls(entries: &[Entry]) -> Vec<Dangling> {
    let finished: HashSet<u64> = entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolResult { call, .. } | Body::ModelResult { call, .. } => Some(*call),
            _ => None,
        })
        .collect();
    let mut dangling: Vec<Dangling> = Vec::new();
    for e in entries {
        let call = match &e.body {
            Body::ToolProcess { call, pgid, boot } => {
                if let Some(d) = dangling.iter_mut().rev().find(|d| d.seq == *call) {
                    d.processes.push((*pgid, boot.clone()));
                }
                continue;
            }
            _ if finished.contains(&e.seq) => continue,
            Body::ToolCall { tool, input, .. } => Call::Tool {
                tool: tool.clone(),
                input: input.clone(),
            },
            Body::ModelCall { .. } => Call::Model,
            _ => continue,
        };
        dangling.push(Dangling {
            seq: e.seq,
            call,
            processes: Vec::new(),
        });
    }
    dangling
}
