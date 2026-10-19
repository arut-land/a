//! What a session's log says, read back for the model: the conversation
//! it sees.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use ext::host::Registry;
use llm::{Assistant, Message, Messages, ToolMessage, ToolResult};
use protocol::{Body, Entry, Outcome, Verdict};

/// The conversation the model sees, folded from the log's facts as they
/// commit, so that a model call reads it rather than the log. A model
/// reply is the assistant message it stored, provider state included. A
/// tool call the model made is answered under the model's id for it, as
/// the tools of the registry it was folded on present the result. The
/// system prompt leads, then the session's context, wherever it was logged.
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
        let message = match &e.body {
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
                    None => llm::Response::from_json(output.raw()).map(|r| r.message),
                };
                match reply {
                    Ok(message) => Message::Assistant(message),
                    Err(_) => return,
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
