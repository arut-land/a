//! What a session's log says, read back: the conversation the model sees,
//! the agent's state, what waits for the user or the agent, and a recap.

use std::borrow::Borrow;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use agent::{Action, Agent, Input, Registry};
use llm::{Assistant, Message, Messages, ToolMessage, ToolResult};
use protocol::ledger::Ledger;
use protocol::{Body, DaemonMsg, Decision, Entry, Outcome, Recap};

/// The conversation the model sees, folded from the log's facts as they
/// commit, so that a model call reads it rather than the log. A model
/// reply is the assistant message it stored, provider state included; a
/// reply logged as bare text (by an earlier version) reads back as text. A
/// tool call the model made is answered under the model's id for it, as
/// the tools of the registry it was folded on present the result. The
/// system prompt leads, then the session's context, wherever it was logged.
/// Requests share the messages, which keep what each encoded of them.
pub struct Transcript {
    messages: Messages,
    /// Where the next context message goes: after the system prompt and
    /// the context before it.
    context: usize,
    /// The model's id and the tool of each call the model made and that is
    /// not yet answered, by seq.
    calls: HashMap<u64, (String, String)>,
    /// The seq of the last entry that changed the messages.
    last: u64,
    registry: Arc<Registry>,
}

impl Transcript {
    /// A transcript on `registry` that `system` prompts.
    pub fn new(registry: Arc<Registry>, system: &str) -> Self {
        let messages = Messages::from_iter((!system.is_empty()).then(|| Message::system(system)));
        Self {
            context: messages.len(),
            messages,
            calls: HashMap::new(),
            last: 0,
            registry,
        }
    }

    /// The transcript of `entries`, on `registry`, that `system` prompts.
    pub fn of(
        entries: impl IntoIterator<Item = impl Borrow<Entry>>,
        registry: Arc<Registry>,
        system: &str,
    ) -> Self {
        let mut transcript = Self::new(registry, system);
        for e in entries {
            transcript.apply(e.borrow(), None);
        }
        transcript
    }

    /// Folds entry `e`. `fresh` is the message of the reply this daemon
    /// just had, which `e` stores: taken as it is rather than read again.
    pub fn apply(&mut self, e: &Entry, fresh: Option<Assistant>) {
        let message = match &e.body {
            Body::User { text }
            | Body::Steer { text, .. }
            | Body::Hook {
                decision: Decision::OnStop { text },
                ..
            } => Message::user(text.clone()),
            Body::Assistant { text } => Message::assistant(text.clone()),
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
                    Ok(message) => Message::Assistant(message),
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
                // Only a tool message changes to take the result.
                if let Some(Message::Tool(_)) = self.messages.last()
                    && let Some(Message::Tool(answers)) = self.messages.last_mut()
                {
                    answers.results.push(result);
                } else {
                    self.messages.push(Message::Tool(ToolMessage {
                        results: vec![result],
                    }));
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
    pub fn before(&self, before: u64) -> Option<&Messages> {
        (self.last < before).then_some(&self.messages)
    }

    pub fn into_messages(self) -> Messages {
        self.messages
    }
}

/// Steps a fresh agent through every input the log records, an entry at a
/// time, so it is in the state it was in when the log was written. What it
/// asked for that the log does not show is left over: the daemon stopped
/// between an outcome and the intents it led to.
#[derive(Default)]
pub struct Replayer(VecDeque<Action>);

impl Replayer {
    pub fn apply(&mut self, agent: &mut dyn Agent, e: &Entry) {
        let asked = &mut self.0;
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
                return;
            }
            // The turn was cut short: what it asked for is not done.
            Body::Interrupt => {
                asked.clear();
                return;
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
            | Body::CommandResult { .. } => return,
        };
        asked.extend(agent.step(input));
    }

    /// What the agent asked for that the log does not show.
    pub fn into_actions(self) -> VecDeque<Action> {
        self.0
    }
}

/// [`Replayer`] over `entries`.
pub fn replay(
    agent: &mut dyn Agent,
    entries: impl IntoIterator<Item = impl Borrow<Entry>>,
) -> VecDeque<Action> {
    let mut replay = Replayer::default();
    for e in entries {
        replay.apply(agent, e.borrow());
    }
    replay.into_actions()
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

/// The answer of the last reply among `entries`, as a recap tells it: an
/// assistant message, or a model reply with text.
pub fn reply(entries: impl IntoIterator<Item = impl Borrow<Entry>>) -> Option<String> {
    let mut reply = None;
    for e in entries {
        match &e.borrow().body {
            Body::Assistant { text } => reply = Some(text.clone()),
            Body::ModelResult {
                outcome: Outcome::Done { output },
                ..
            } => {
                let text = llm::Response::from_value(&output.value()).map(|r| r.message.text());
                if let Ok(text) = text
                    && !text.is_empty()
                {
                    reply = Some(text);
                }
            }
            _ => {}
        }
    }
    reply
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
