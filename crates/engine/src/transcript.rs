//! What a session's log says, read back for the model and for clients that
//! fold no ledger: the conversation the model sees, the asks still
//! waiting, and a recap.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use ext::host::Registry;
use llm::{Assistant, Message, Messages, ToolMessage, ToolResult};
use protocol::ledger::Ledger;
use protocol::{Body, DaemonMsg, Entry, Outcome, Recap, Verdict};

/// The conversation the model sees, folded from the log's facts as they
/// commit, so that a model call reads it rather than the log. A model
/// reply is the assistant message it stored, provider state included; a
/// reply logged as bare text (by an earlier version) reads back as text. A
/// tool call the model made is answered under the model's id for it, as
/// the tools of the registry it was folded on present the result. The
/// system prompt leads, then the session's context, wherever it was logged.
/// It folds each entry as the v2 kinds say it, so old logs read the same.
pub struct Transcript {
    messages: Messages,
    /// Where the next context message goes: after the system prompt and
    /// the context before it.
    context: usize,
    /// The model's id and the tool of each call the model made and that is
    /// not yet answered, by seq.
    calls: HashMap<u64, (String, String)>,
    /// Input waiting to be taken up, by seq: the user's once it is.
    waiting: HashMap<u64, String>,
    /// The seq of the last entry that changed the messages.
    last: u64,
    registry: Arc<Registry>,
}

impl Transcript {
    /// The transcript of `entries`, on `registry`, that `system` prompts.
    pub fn of(
        entries: impl IntoIterator<Item = impl Borrow<Entry>>,
        registry: Arc<Registry>,
        system: &str,
    ) -> Self {
        let messages = Messages::from_iter((!system.is_empty()).then(|| Message::system(system)));
        let mut transcript = Self {
            context: messages.len(),
            messages,
            calls: HashMap::new(),
            waiting: HashMap::new(),
            last: 0,
            registry,
        };
        for e in entries {
            transcript.apply(e.borrow(), None);
        }
        transcript
    }

    /// Folds entry `e`. `fresh` is the message of the reply this daemon
    /// just had, which `e` stores: taken as it is rather than read again.
    pub fn apply(&mut self, e: &Entry, fresh: Option<Assistant>) {
        let Some(body) = e.body.shim() else { return };
        let message = match &*body {
            Body::Input {
                text, taken: true, ..
            }
            | Body::Decision {
                d: Verdict::OnStop { text },
                ..
            } => Message::user(text.clone()),
            Body::Input { text, .. } => {
                self.waiting.insert(e.seq, text.clone());
                return;
            }
            Body::Take { input } => match self.waiting.remove(&input.get()) {
                Some(text) => Message::user(text),
                None => return,
            },
            Body::Assistant { text } => Message::assistant(text.clone()),
            Body::Context { text } => {
                self.messages
                    .insert(self.context, Message::user(text.clone()));
                self.context += 1;
                self.last = e.seq;
                return;
            }
            Body::Fired { outcome, .. } => Message::user(ext::child_result(outcome)),
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
                if let Some(Message::Tool(answers)) = self.messages.last_mut() {
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
fn short(text: &str) -> String {
    const WIDTH: usize = 60;
    let text = text.trim();
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(WIDTH) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None if line.len() < text.len() => format!("{line}…"),
        None => line.to_owned(),
    }
}
