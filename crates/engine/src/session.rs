use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use agent::{Action, Agent, Chat, Cx, Echo, Input, Replay, Toolset};

use crate::model::Model;
use protocol::{Body, DaemonMsg, Entry, Outcome, SessionId, SessionSummary};
use serde_json::Value;
use store::{Loaded, Log, Store};

/// What every session runs with.
pub struct Runtime {
    pub agent: AgentKind,
    pub tools: Toolset,
    /// The model, or why there is none (shown as the failed model call).
    pub model: Result<Model, String>,
    pub pins: Pins,
}

/// What keeps the daemon alive: attached clients and unfinished work.
#[derive(Default)]
pub struct Pins {
    clients: AtomicUsize,
    work: AtomicUsize,
}

impl Pins {
    pub fn idle(&self) -> bool {
        self.clients.load(Ordering::SeqCst) == 0 && self.work.load(Ordering::SeqCst) == 0
    }

    pub fn client(&self) -> PinGuard<'_> {
        PinGuard::new(&self.clients)
    }

    fn work(&self) -> PinGuard<'_> {
        PinGuard::new(&self.work)
    }
}

pub struct PinGuard<'a>(&'a AtomicUsize);

impl<'a> PinGuard<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for PinGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
pub enum AgentKind {
    Echo,
    Chat,
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
    rt: Arc<Runtime>,
    store: Arc<Store>,
}

enum Msg {
    Input(String),
    Stop,
}

struct State {
    entries: Vec<Entry>,
    log: Log,
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
    /// Loads a session and starts its actor: the single writer that steps
    /// the agent and runs its effects. Calls left dangling by a crash are
    /// recovered first.
    pub fn start(
        id: SessionId,
        workspace: PathBuf,
        loaded: Loaded,
        rt: Arc<Runtime>,
        store: Arc<Store>,
    ) -> Arc<Self> {
        let (inbox, rx) = mpsc::channel();
        let state = Mutex::new(State {
            entries: loaded.entries,
            log: loaded.log,
            clients: Vec::new(),
        });
        let session = Arc::new(Self {
            id,
            workspace,
            state,
            inbox,
            // Recovery counts as queued work until it has run.
            queued: AtomicUsize::new(1),
            attached: AtomicUsize::new(0),
            last_active: Mutex::new(Instant::now()),
            rt,
            store,
        });
        let actor = session.clone();
        thread::spawn(move || actor.run(&rx));
        session
    }

    /// Opens a client attachment. Callers hold the session's slot lock, so a
    /// session cannot be unloaded between being found and being attached.
    pub fn open(self: &Arc<Self>) -> Attachment {
        self.attached.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Attachment(self.clone())
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
        self.last_active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
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

    pub fn summary(&self) -> SessionSummary {
        let st = self.lock();
        SessionSummary {
            id: self.id,
            workspace: self.workspace.clone(),
            entries: st.entries.len() as u64,
            clients: u32::try_from(self.attached.load(Ordering::SeqCst)).unwrap_or(u32::MAX),
        }
    }

    fn touch(&self) {
        *self
            .last_active
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
        st.entries.push(entry);
        Ok(seq)
    }

    /// Sends a message that is not part of the log to every attached client.
    fn broadcast(&self, msg: &DaemonMsg) {
        self.lock().clients.retain(|c| c.send(msg.clone()).is_ok());
    }

    fn run(&self, inbox: &Receiver<Msg>) {
        let rt = &*self.rt;
        let mut agent: Box<dyn Agent> = match rt.agent {
            AgentKind::Echo => Box::new(Echo),
            AgentKind::Chat => Box::new(Chat),
        };
        self.work(|| self.recover(agent.as_mut(), rt));
        for msg in inbox {
            let Msg::Input(text) = msg else { return };
            self.work(|| {
                self.append(Body::User { text: text.clone() })?;
                self.drive(agent.as_mut(), rt, Input::User { text })
            });
        }
    }

    /// Runs one queued unit of work, with the daemon pinned and a busy marker
    /// on disk for as long as anything is in flight.
    fn work(&self, job: impl FnOnce() -> io::Result<()>) {
        let _pinned = self.rt.pins.work();
        if let Err(e) = self.store.mark_busy(self.id, true) {
            eprintln!("session {}: busy marker: {e}", self.id.short());
        }
        if let Err(e) = job() {
            eprintln!("session {}: {e}", self.id.short());
        }
        if self.queued.fetch_sub(1, Ordering::SeqCst) == 1
            && let Err(e) = self.store.mark_busy(self.id, false)
        {
            eprintln!("session {}: busy marker: {e}", self.id.short());
        }
        self.touch();
    }

    fn drive(&self, agent: &mut dyn Agent, rt: &Runtime, input: Input) -> io::Result<()> {
        let mut inputs = VecDeque::from([input]);
        while let Some(input) = inputs.pop_front() {
            for action in agent.step(input) {
                match action {
                    Action::Emit(body) => {
                        self.append(body)?;
                    }
                    Action::RunTool { tool, input } => {
                        let call = self.append(Body::ToolCall {
                            tool: tool.clone(),
                            input: input.clone(),
                        })?;
                        let outcome = self.run_tool(&rt.tools, call, &tool, &input);
                        self.append(Body::ToolResult {
                            call,
                            outcome: outcome.clone(),
                        })?;
                        inputs.push_back(Input::ToolDone { call, outcome });
                    }
                    Action::CallModel => {
                        let model = rt
                            .model
                            .as_ref()
                            .map_or_else(|_| "none".to_owned(), Model::name);
                        let call = self.append(Body::ModelCall { model })?;
                        let outcome = self.run_model(rt, call);
                        self.append(Body::ModelResult {
                            call,
                            outcome: outcome.clone(),
                        })?;
                        inputs.push_back(Input::ModelDone { call, outcome });
                    }
                }
            }
        }
        Ok(())
    }

    fn run_tool(&self, tools: &Toolset, call: u64, tool: &str, input: &Value) -> Outcome {
        let spawned = |pgid| {
            let Some(boot) = sys::boot_id() else { return };
            let body = Body::ToolProcess {
                call,
                pgid,
                boot: boot.to_owned(),
            };
            if let Err(e) = self.append(body) {
                eprintln!("session {}: recording process {pgid}: {e}", self.id.short());
            }
        };
        let cx = Cx {
            cwd: &self.workspace,
            spawned: &spawned,
        };
        contain(|| tools.run(tool, input, &cx))
    }

    /// Asks the model to reply to the transcript before `call`. Text streams to
    /// clients as it arrives; only the finished reply is committed.
    fn run_model(&self, rt: &Runtime, call: u64) -> Outcome {
        let model = match &rt.model {
            Ok(model) => model,
            Err(error) => {
                return Outcome::Failed {
                    error: error.clone(),
                };
            }
        };
        let history = transcript(&self.lock().entries, call);
        let mut on_text = |text: &str| {
            self.broadcast(&DaemonMsg::Delta {
                call,
                text: text.to_owned(),
            });
        };
        // The whole reply is the fact: replaying it later sends the
        // provider's own items (signatures, encrypted reasoning) back.
        contain(|| match model.reply(history, &mut on_text) {
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

    /// Finishes the effect sandwich for calls that have an intent but no
    /// outcome: leftover processes are killed, then safe calls (model calls,
    /// safe tools) run again and everything else is marked interrupted.
    fn recover(&self, agent: &mut dyn Agent, rt: &Runtime) -> io::Result<()> {
        let dangling = dangling_calls(&self.lock().entries);
        for call in dangling {
            let this_boot = call
                .processes
                .iter()
                .filter(|(_, boot)| Some(boot.as_str()) == sys::boot_id());
            for (pgid, _) in this_boot {
                if let Err(e) = sys::kill_process_group(*pgid) {
                    eprintln!(
                        "session {}: killing process group {pgid}: {e}",
                        self.id.short()
                    );
                }
            }
            match call.kind {
                Pending::Model => {
                    let outcome = self.run_model(rt, call.seq);
                    self.append(Body::ModelResult {
                        call: call.seq,
                        outcome: outcome.clone(),
                    })?;
                    self.drive(
                        agent,
                        rt,
                        Input::ModelDone {
                            call: call.seq,
                            outcome,
                        },
                    )?;
                }
                Pending::Tool { tool, input } => {
                    let outcome = match rt.tools.get(&tool).map(agent::Tool::replay) {
                        Some(Replay::Safe) => self.run_tool(&rt.tools, call.seq, &tool, &input),
                        _ => Outcome::Interrupted,
                    };
                    self.append(Body::ToolResult {
                        call: call.seq,
                        outcome: outcome.clone(),
                    })?;
                    self.drive(
                        agent,
                        rt,
                        Input::ToolDone {
                            call: call.seq,
                            outcome,
                        },
                    )?;
                }
            }
        }
        Ok(())
    }
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

/// The conversation the model sees, derived from the log's facts. A model
/// reply is the assistant message it stored; a reply logged as bare text (by
/// an earlier version) reads back as text.
fn transcript(entries: &[Entry], before: u64) -> Vec<llm::Message> {
    entries
        .iter()
        .take_while(|e| e.seq < before)
        .filter_map(|e| match &e.body {
            Body::User { text } => Some(llm::Message::user(text.clone())),
            Body::Assistant { text } => Some(llm::Message::assistant(text.clone())),
            Body::ModelResult {
                outcome: Outcome::Done { output },
                ..
            } => match serde_json::from_value::<llm::Response>(output.clone()) {
                Ok(reply) => Some(llm::Message::Assistant(reply.message)),
                Err(_) => Some(llm::Message::assistant(output["text"].as_str()?.to_owned())),
            },
            _ => None,
        })
        .collect()
}

enum Pending {
    Model,
    Tool { tool: String, input: Value },
}

struct Dangling {
    seq: u64,
    kind: Pending,
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
        let kind = match &e.body {
            Body::ToolProcess { call, pgid, boot } => {
                if let Some(d) = dangling.iter_mut().find(|d| d.seq == *call) {
                    d.processes.push((*pgid, boot.clone()));
                }
                continue;
            }
            _ if finished.contains(&e.seq) => continue,
            Body::ToolCall { tool, input } => Pending::Tool {
                tool: tool.clone(),
                input: input.clone(),
            },
            Body::ModelCall { .. } => Pending::Model,
            _ => continue,
        };
        dangling.push(Dangling {
            seq: e.seq,
            kind,
            processes: Vec::new(),
        });
    }
    dangling
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(seq: u64, body: Body) -> Entry {
        Entry {
            seq,
            epoch: 0,
            body,
        }
    }

    #[test]
    fn finds_calls_without_outcomes() {
        let entries = [
            entry(
                1,
                Body::ToolCall {
                    tool: "sleep".into(),
                    input: json!({}),
                },
            ),
            entry(
                2,
                Body::ToolResult {
                    call: 1,
                    outcome: Outcome::Interrupted,
                },
            ),
            entry(
                3,
                Body::ToolCall {
                    tool: "run_command".into(),
                    input: json!({}),
                },
            ),
            entry(
                4,
                Body::ToolProcess {
                    call: 3,
                    pgid: 42,
                    boot: "boot".into(),
                },
            ),
            entry(5, Body::ModelCall { model: "m".into() }),
        ];
        let dangling = dangling_calls(&entries);
        assert_eq!(dangling.len(), 2);
        assert_eq!(dangling[0].seq, 3);
        assert_eq!(dangling[0].processes, [(42, "boot".to_owned())]);
        assert!(matches!(dangling[1].kind, Pending::Model));
    }

    #[test]
    fn transcript_is_derived_from_facts() {
        let done = Outcome::Done {
            output: json!({ "text": "hello" }),
        };
        let entries = [
            entry(1, Body::User { text: "hi".into() }),
            entry(2, Body::ModelCall { model: "m".into() }),
            entry(
                3,
                Body::ModelResult {
                    call: 2,
                    outcome: done,
                },
            ),
            entry(
                4,
                Body::User {
                    text: "again".into(),
                },
            ),
            entry(5, Body::ModelCall { model: "m".into() }),
        ];
        let turns = transcript(&entries, 5);
        let got: Vec<_> = turns
            .iter()
            .map(|message| match message {
                llm::Message::User(user) => match &user.content[..] {
                    [llm::UserPart::Text(text)] => ("user", text.text.clone()),
                    _ => ("user", String::new()),
                },
                llm::Message::Assistant(assistant) => ("assistant", assistant.text()),
                _ => ("other", String::new()),
            })
            .collect();
        assert_eq!(
            got,
            [
                ("user", "hi".to_owned()),
                ("assistant", "hello".to_owned()),
                ("user", "again".to_owned())
            ]
        );
    }
}
