//! What a session's log says, read back for the model: the conversation
//! it sees.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use ext::host::Registry;
use llm::{Assistant, Message, Messages, ToolMessage, ToolResult};
use protocol::{Body, CallId, Entry, Outcome, Seq, Verdict};

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
    calls: HashMap<CallId, (String, String)>,
    /// The calls the last message answers, by seq, in the order it does:
    /// the calls' order, whatever order they ended in, so the request is
    /// the same however a run went.
    answered: Vec<CallId>,
    /// Input waiting to be taken up, by seq: the user's once it is.
    waiting: HashMap<Seq, String>,
    /// The seq of the last entry that changed the messages.
    last: u64,
    registry: Arc<Registry>,
}

impl Transcript {
    /// The transcript of `entries`, on `registry`, that `system` prompts,
    /// as their edits leave it: the entries an `edit` covers stand as its
    /// text, a user's message, or are left out; the latest edit of an
    /// entry is what stands.
    pub fn of(
        entries: impl IntoIterator<Item = impl Borrow<Entry>>,
        registry: Arc<Registry>,
        system: &str,
    ) -> Self {
        let entries: Vec<_> = entries.into_iter().collect();
        let edits: Vec<(u64, u64, Option<String>)> = (entries.iter())
            .filter_map(|e| match &e.borrow().body {
                Body::Edit {
                    target,
                    upto,
                    replace,
                } => {
                    let text = replace.as_ref().and_then(|j| j.parse::<String>().ok());
                    Some((target.get(), upto.unwrap_or(*target).get(), text))
                }
                _ => None,
            })
            .collect();
        let messages = Messages::from_iter((!system.is_empty()).then(|| Message::system(system)));
        let mut transcript = Self {
            context: messages.len(),
            messages,
            calls: HashMap::new(),
            answered: Vec::new(),
            waiting: HashMap::new(),
            last: 0,
            registry,
        };
        for e in &entries {
            let e = e.borrow();
            let edit = edits
                .iter()
                .rev()
                .find(|(from, to, _)| (*from..=*to).contains(&e.seq.get()));
            match edit {
                None => transcript.apply(e, None),
                Some((from, _, Some(text))) if *from == e.seq.get() => {
                    transcript.messages.push(Message::user(text.clone()));
                    transcript.last = e.seq.get();
                }
                Some(_) => {}
            }
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
            Body::Take { input } => match self.waiting.remove(input) {
                Some(text) => Message::user(text),
                None => return,
            },
            Body::Assistant { text } => Message::assistant(text.clone()),
            Body::Context { text } => {
                self.messages
                    .insert(self.context, Message::user(text.clone()));
                self.context += 1;
                self.last = e.seq.get();
                return;
            }
            Body::Fired { outcome, .. } => Message::user(ext::child_result(outcome)),
            Body::ModelResult { r#for: Some(_), .. } => return,
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
                self.calls.insert(CallId(e.seq), (id.clone(), tool.clone()));
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
                self.last = e.seq.get();
                // Only a tool message changes to take the result.
                if let Some(Message::Tool(answers)) = self.messages.last_mut() {
                    let at = self.answered.partition_point(|&seq| seq < *call);
                    self.answered.insert(at, *call);
                    answers.results.insert(at, result);
                } else {
                    self.answered = vec![*call];
                    self.messages.push(Message::Tool(ToolMessage {
                        results: vec![result],
                    }));
                }
                return;
            }
            _ => return,
        };
        self.messages.push(message);
        self.last = e.seq.get();
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
