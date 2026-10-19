//! What a session's log says, read back: the conversation the model sees,
//! the agent's state, what waits for the user or the agent, and a recap.

use std::borrow::Borrow;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use agent::{Action, Agent, Input, Registry};
use llm::{Assistant, Message, ToolMessage, ToolResult};
use protocol::ledger::Ledger;
use protocol::{Body, DaemonMsg, Decision, Entry, Outcome, Recap};

/// The conversation the model sees, folded from the log's facts as they
/// commit, so that a model call reads it rather than the log. A model
/// reply is the assistant message it stored, provider state included; a
/// reply logged as bare text (by an earlier version) reads back as text. A
/// tool call the model made is answered under the model's id for it, as
/// the tools of the registry it was folded on present the result. The
/// session's context comes first, wherever it was logged.
pub struct Transcript {
    messages: Vec<Message>,
    /// How many context messages lead `messages`.
    context: usize,
    /// The model's id and the tool of each call the model made and that is
    /// not yet answered, by seq.
    calls: HashMap<u64, (String, String)>,
    /// The seq of the last entry that changed the messages.
    last: u64,
    /// The answer of the current turn's last reply, as a recap tells it.
    reply: Option<String>,
    registry: Arc<Registry>,
}

impl Transcript {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            messages: Vec::new(),
            context: 0,
            calls: HashMap::new(),
            last: 0,
            reply: None,
            registry,
        }
    }

    /// The transcript of `entries`, on `registry`.
    pub fn of(
        entries: impl IntoIterator<Item = impl Borrow<Entry>>,
        registry: Arc<Registry>,
    ) -> Self {
        let mut transcript = Self::new(registry);
        for e in entries {
            transcript.apply(e.borrow(), None);
        }
        transcript
    }

    /// Folds entry `e`. `fresh` is the message of the reply this daemon
    /// just had, which `e` stores: taken as it is rather than read again.
    pub fn apply(&mut self, e: &Entry, fresh: Option<Assistant>) {
        let message = match &e.body {
            Body::User { text } => {
                self.reply = None;
                Message::user(text.clone())
            }
            Body::Steer { text, .. }
            | Body::Hook {
                decision: Decision::OnStop { text },
                ..
            } => Message::user(text.clone()),
            Body::Assistant { text } => {
                self.reply = Some(text.clone());
                Message::assistant(text.clone())
            }
            Body::Context { text } => {
                self.messages
                    .insert(self.context, Message::user(text.clone()));
                self.context += 1;
                self.last = e.seq;
                return;
            }
            Body::Fired { outcome, .. } => Message::user(agent::child_result(outcome)),
            Body::ModelResult {
                outcome: Outcome::Done { output },
                ..
            } => {
                let reply = match fresh {
                    Some(message) => Ok(message),
                    None => llm::Response::from_value(&output.value()).map(|r| r.message),
                };
                match reply {
                    Ok(message) => {
                        let text = message.text();
                        if !text.is_empty() {
                            self.reply = Some(text);
                        }
                        Message::Assistant(message)
                    }
                    // A reply an earlier version logged as bare text.
                    Err(_) => match output.value()["text"].as_str() {
                        Some(text) => Message::assistant(text),
                        None => return,
                    },
                }
            }
            Body::ToolCall {
                tool, id: Some(id), ..
            } => {
                self.calls.insert(e.seq, (id.clone(), tool.clone()));
                return;
            }
            Body::ToolResult { call, outcome, .. } => {
                let Some((id, tool)) = self.calls.remove(call) else {
                    return;
                };
                let (output, error) = self.registry.tools().to_model(&tool, outcome);
                let result = ToolResult {
                    id: id.into(),
                    name: tool.into(),
                    output,
                    error,
                    meta: llm::Meta::new(),
                };
                self.last = e.seq;
                match self.messages.last_mut() {
                    Some(Message::Tool(answers)) => answers.results.push(result),
                    _ => self.messages.push(Message::Tool(ToolMessage {
                        results: vec![result],
                    })),
                }
                return;
            }
            _ => return,
        };
        self.messages.push(message);
        self.last = e.seq;
    }

    /// Whether the tools of `registry` presented the results.
    pub fn is_on(&self, registry: &Arc<Registry>) -> bool {
        Arc::ptr_eq(&self.registry, registry)
    }

    /// The conversation before entry `before`, if it is all of it.
    pub fn before(&self, before: u64) -> Option<&[Message]> {
        (self.last < before).then_some(&self.messages[..])
    }

    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }

    /// The answer of the current turn's last reply.
    pub fn reply(&self) -> Option<&str> {
        self.reply.as_deref()
    }
}

/// Steps a fresh `agent` through every input the log records, so it is in
/// the state it was in when the log was written. Returns what it asked for
/// that the log does not show: the daemon stopped between an outcome and the
/// intents it led to.
pub fn replay(agent: &mut dyn Agent, entries: &[Entry]) -> VecDeque<Action> {
    let mut asked = VecDeque::new();
    for e in entries {
        let input = match &e.body {
            Body::User { text }
            | Body::Hook {
                decision: Decision::OnStop { text },
                ..
            } => Input::User { text: text.clone() },
            Body::ToolResult { call, outcome, .. } => Input::ToolDone {
                call: *call,
                outcome: outcome.clone(),
            },
            Body::ModelResult { call, outcome } => Input::ModelDone {
                call: *call,
                outcome: outcome.clone(),
            },
            Body::Fired { watch, outcome } => Input::ChildDone {
                watch: *watch,
                outcome: outcome.clone(),
            },
            Body::Assistant { .. }
            | Body::ToolCall { .. }
            | Body::ModelCall { .. }
            | Body::Link { .. } => {
                // A log that disagrees with the agent (an earlier version
                // wrote it) leaves nothing to redo.
                if asked.front().is_some_and(|action| commits(action, &e.body)) {
                    asked.pop_front();
                } else {
                    asked.clear();
                }
                continue;
            }
            // The turn was cut short: what it asked for is not done.
            Body::Interrupt => {
                asked.clear();
                continue;
            }
            // Read by the model, not stepped through the agent.
            Body::Steer { .. }
            | Body::Queued { .. }
            | Body::Progress { .. }
            | Body::Settings(_)
            | Body::ToolProcess { .. }
            | Body::Hook { .. }
            | Body::Ask { .. }
            | Body::Watch { .. }
            | Body::Environment(_)
            | Body::Extensions { .. }
            | Body::Ext { .. }
            | Body::Context { .. }
            | Body::Command { .. }
            | Body::CommandResult { .. } => continue,
        };
        asked.extend(agent.step(input));
    }
    asked
}

/// Whether `body` is the entry carrying out `action`.
fn commits(action: &Action, body: &Body) -> bool {
    match (action, body) {
        (Action::Emit(emitted), body) => emitted == body,
        (Action::RunTool { tool, .. }, Body::ToolCall { tool: called, .. }) => tool == called,
        (Action::CallModel, Body::ModelCall { .. }) => true,
        (Action::Spawn { input }, Body::Link { input: linked, .. }) => input == linked,
        _ => false,
    }
}

/// What a client is sent for the ask about tool call `call`, while it
/// waits.
pub fn ask(ledger: &Ledger, call: u64) -> Option<DaemonMsg> {
    let (tool, input) = ledger.asked(call)?;
    Some(DaemonMsg::Ask {
        call,
        tool: tool.to_owned(),
        input: input.clone(),
    })
}

/// What a client attaching after `since` is told: the user's last message
/// and what the agent did since, `reply` its last answer. `None` when
/// nothing worth telling is new, such as only an environment.
pub fn recap(ledger: &Ledger, reply: Option<&str>, since: u64) -> Option<Recap> {
    if ledger.seq() <= since {
        return None;
    }
    let recap = Recap {
        prompt: ledger.prompt().map(short),
        tools: ledger.turn_tools().to_vec(),
        failed: ledger.turn_failed(),
        running: ledger.turn_running(),
        reply: reply.map(short),
        asks: u32::try_from(ledger.inbox().len()).unwrap_or(u32::MAX),
    };
    (recap != Recap::default()).then_some(recap)
}

/// The first line of `text`, cut to a recap's width.
pub(super) fn short(text: &str) -> String {
    const WIDTH: usize = 60;
    let text = text.trim();
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(WIDTH) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None if line.len() < text.len() => format!("{line}…"),
        None => line.to_owned(),
    }
}
