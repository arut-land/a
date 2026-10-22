//! What a session's log says, read back for the model: the conversation
//! it sees.

use std::borrow::{Borrow, Cow};
use std::collections::HashMap;
use std::sync::Arc;

use ext::host::Registry;
use llm::{Assistant, Message, Messages, ToolMessage, ToolResult};
use protocol::{Body, CallId, Entry, Outcome, Seq, Verdict};

/// Provider replies remain intact; live updates and recovery share projection rules.
pub struct Transcript {
    messages: Messages,
    model_only: bool,
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

// Both the live reducer and immutable images admit context through this seam.
enum ContextFact<'a> {
    Inherited(&'a protocol::Json),
    User(&'a str),
    Queued(&'a str),
    Taken(Seq),
    Assistant(&'a str),
    Context(&'a str),
    Child(&'a Outcome),
    Reply(&'a protocol::Json),
    ToolCall { id: &'a str, tool: &'a str },
    ToolResult { call: CallId, outcome: &'a Outcome },
    Edit,
}

fn context_fact(body: &Body) -> Option<ContextFact<'_>> {
    Some(match body {
        Body::Spawned { intent } => ContextFact::Inherited(intent),
        Body::Input {
            text, taken: true, ..
        }
        | Body::Decision {
            d: Verdict::OnStop { text },
            ..
        } => ContextFact::User(text),
        Body::Input { text, .. } => ContextFact::Queued(text),
        Body::Enqueue { envelope } => ContextFact::Queued(&envelope.payload),
        Body::Take { input } => ContextFact::Taken(*input),
        Body::Assistant { text } => ContextFact::Assistant(text),
        Body::Context { text, .. } => ContextFact::Context(text),
        Body::Fired { outcome, .. } => ContextFact::Child(outcome),
        Body::ModelResult {
            r#for: None,
            outcome: Outcome::Done { output },
            ..
        } => ContextFact::Reply(output),
        Body::ToolCall {
            tool, id: Some(id), ..
        } => ContextFact::ToolCall { id, tool },
        Body::ToolResult { call, outcome, .. } => ContextFact::ToolResult {
            call: *call,
            outcome,
        },
        Body::Edit { .. } => ContextFact::Edit,
        _ => return None,
    })
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
        ContextImage::new(entries.iter().map(|e| Cow::Borrowed(e.borrow())).collect())
            .project(Self::empty(registry, system))
    }

    fn empty(registry: Arc<Registry>, system: &str) -> Self {
        let messages = Messages::from_iter((!system.is_empty()).then(|| Message::system(system)));
        Self {
            context: messages.len(),
            messages,
            model_only: false,
            calls: HashMap::new(),
            answered: Vec::new(),
            waiting: HashMap::new(),
            last: 0,
            registry,
        }
    }

    /// Folds entry `e`. `fresh` is the message of the reply this daemon
    /// just had, which `e` stores: taken as it is rather than read again.
    pub fn apply(&mut self, e: &Entry, fresh: Option<Assistant>) {
        let message = match context_fact(&e.body) {
            Some(ContextFact::Inherited(intent)) => {
                if let Ok(image) = protocol::publication::SpawnImage::from_json(intent) {
                    self.model_only = image.runner == protocol::publication::RunnerKind::ModelOnly;
                    let messages =
                        ContextImage::new(image.entries().into_iter().map(Cow::Owned).collect())
                            .project(Self::empty(Arc::clone(&self.registry), ""))
                            .into_context();
                    for message in messages.iter() {
                        self.messages.push(message.clone());
                    }
                    self.context = self.messages.len();
                    self.last = e.seq.get();
                }
                return;
            }
            Some(ContextFact::User(text)) => Message::user(text.to_owned()),
            Some(ContextFact::Queued(text)) => {
                self.waiting.insert(e.seq, text.to_owned());
                return;
            }
            Some(ContextFact::Taken(input)) => match self.waiting.remove(&input) {
                Some(text) if self.model_only => {
                    if let Ok((turn, prefix)) =
                        serde_json::from_str::<(ext::ModelTurn, Messages)>(&text)
                    {
                        for message in prefix.iter().chain(turn.messages.iter()) {
                            self.messages.push(message.clone());
                        }
                        self.last = e.seq.get();
                        return;
                    }
                    Message::user(text)
                }
                Some(text) => Message::user(text),
                None => return,
            },
            Some(ContextFact::Assistant(text)) => Message::assistant(text.to_owned()),
            Some(ContextFact::Context(text)) => {
                self.messages
                    .insert(self.context, Message::user(text.to_owned()));
                self.context += 1;
                self.last = e.seq.get();
                return;
            }
            Some(ContextFact::Child(outcome)) => Message::user(ext::child_result(outcome)),
            Some(ContextFact::Reply(output)) => {
                let reply = match fresh {
                    Some(message) => Ok(message),
                    None => llm::Response::from_json(output.raw()).map(|r| r.message),
                };
                match reply {
                    Ok(message) => Message::Assistant(message),
                    Err(_) => return,
                }
            }
            Some(ContextFact::ToolCall { id, tool }) => {
                self.calls
                    .insert(CallId(e.seq), (id.to_owned(), tool.to_owned()));
                return;
            }
            Some(ContextFact::ToolResult { call, outcome }) => {
                let Some((id, tool)) = self.calls.remove(&call) else {
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
                    let at = self.answered.partition_point(|&seq| seq < call);
                    self.answered.insert(at, call);
                    answers.results.insert(at, result);
                } else {
                    self.answered = vec![call];
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

    /// Pending exchanges cannot be replayed by a context-only child.
    pub fn into_context(self) -> Messages {
        let mut messages: Vec<_> = self.messages.iter().cloned().collect();
        let mut requested = HashMap::new();
        let mut answered: HashMap<usize, std::collections::HashSet<_>> = HashMap::new();
        for (at, message) in messages.iter_mut().enumerate() {
            match message {
                Message::Assistant(a) => {
                    requested.extend(a.tool_calls().map(|c| (c.id.clone(), at)));
                }
                Message::Tool(t) => t.results.retain(|r| {
                    if let Some(owner) = requested.remove(&r.id) {
                        answered.entry(owner).or_default().insert(r.id.clone());
                        true
                    } else {
                        false
                    }
                }),
                _ => {}
            }
        }
        for (at, message) in messages.iter_mut().enumerate() {
            if let Message::Assistant(a) = message {
                a.content.retain(|p| !matches!(p, llm::AssistantPart::ToolCall(c) if !c.provider_executed && !answered.get(&at).is_some_and(|ids| ids.contains(&c.id))));
            }
        }
        messages
            .into_iter()
            .filter(|m| match m {
                Message::Assistant(a) => !a.content.is_empty(),
                Message::Tool(t) => !t.results.is_empty(),
                _ => true,
            })
            .collect()
    }

    pub fn into_messages(self) -> Messages {
        self.messages
    }
}

fn context_edits(
    entries: impl IntoIterator<Item = impl Borrow<Entry>>,
) -> Vec<(u64, u64, Option<String>)> {
    entries
        .into_iter()
        .filter_map(|e| match &e.borrow().body {
            Body::Edit {
                target,
                upto,
                replace,
            } => Some((
                target.get(),
                upto.unwrap_or(*target).get(),
                replace.as_ref().and_then(|j| j.parse::<String>().ok()),
            )),
            _ => None,
        })
        .collect()
}

/// Sequence numbers belong to their layer; inherited authority is never projected.
struct ContextImage<'a> {
    layers: Vec<Vec<ContextInput<'a>>>,
}

enum ContextInput<'a> {
    Entry(Cow<'a, Entry>),
    Replacement { seq: Seq, text: String },
    Inherited { seq: Seq, layer: usize },
}

impl<'a> ContextImage<'a> {
    fn new(entries: Vec<Cow<'a, Entry>>) -> Self {
        let mut pending = vec![entries];
        let mut layers = Vec::new();
        let mut at = 0;
        while at < pending.len() {
            let entries = std::mem::take(&mut pending[at]);
            let edits = context_edits(entries.iter().map(AsRef::as_ref));
            let mut inputs = Vec::new();
            for entry in entries {
                let edit = edits
                    .iter()
                    .rev()
                    .find(|(from, to, _)| (*from..=*to).contains(&entry.seq.get()));
                if let Some((from, _, text)) = edit {
                    if *from == entry.seq.get()
                        && let Some(text) = text
                    {
                        inputs.push(ContextInput::Replacement {
                            seq: entry.seq,
                            text: text.clone(),
                        });
                    }
                    continue;
                }
                if let Body::Spawned { intent } = &entry.body {
                    if let Ok(image) = protocol::publication::SpawnImage::from_json(intent) {
                        if image.runner == protocol::publication::RunnerKind::ModelOnly {
                            inputs.push(ContextInput::Entry(entry));
                            continue;
                        }
                        inputs.push(ContextInput::Inherited {
                            seq: entry.seq,
                            layer: pending.len(),
                        });
                        pending.push(image.entries().into_iter().map(Cow::Owned).collect());
                    }
                } else if context_fact(&entry.body).is_some() {
                    inputs.push(ContextInput::Entry(entry));
                }
            }
            layers.push(inputs);
            at += 1;
        }
        Self { layers }
    }

    fn project(self, root: Transcript) -> Transcript {
        let mut root = Some(root);
        let mut inherited = HashMap::new();
        for (layer, inputs) in self.layers.into_iter().enumerate().rev() {
            let mut transcript = if layer == 0 {
                root.take().expect("root reduced once")
            } else {
                Transcript::empty(
                    Arc::clone(&root.as_ref().expect("root reduced last").registry),
                    "",
                )
            };
            for input in inputs {
                match input {
                    ContextInput::Entry(entry) => transcript.apply(&entry, None),
                    ContextInput::Replacement { seq, text } => {
                        transcript.messages.push(Message::user(text));
                        transcript.last = seq.get();
                    }
                    ContextInput::Inherited { seq, layer } => {
                        let messages: Messages =
                            inherited.remove(&layer).expect("inherited layer reduced");
                        for message in messages.iter() {
                            transcript.messages.push(message.clone());
                        }
                        transcript.context = transcript.messages.len();
                        transcript.last = seq.get();
                    }
                }
            }
            if layer == 0 {
                return transcript;
            }
            inherited.insert(layer, transcript.into_context());
        }
        unreachable!("context has a root layer")
    }
}

/// Stored projection facts carry no executable child ownership.
pub(crate) fn context_image(entries: &[Entry]) -> Vec<protocol::Json> {
    let completed: std::collections::HashSet<_> = entries
        .iter()
        .filter_map(|e| match context_fact(&e.body) {
            Some(ContextFact::ToolResult { call, .. }) => Some(call.0),
            _ => None,
        })
        .collect();
    let taken: std::collections::HashSet<_> = entries
        .iter()
        .filter_map(|e| match context_fact(&e.body) {
            Some(ContextFact::Taken(input)) => Some(input),
            _ => None,
        })
        .collect();
    let edits = context_edits(entries);
    entries
        .iter()
        .filter_map(|e| {
            if let Some((from, _, text)) = edits
                .iter()
                .rev()
                .find(|(from, to, _)| (*from..=*to).contains(&e.seq.get()))
            {
                return (*from == e.seq.get())
                    .then_some(text.as_ref())
                    .flatten()
                    .map(|text| {
                        protocol::Json::encode(&Entry::new(
                            e.seq,
                            e.epoch,
                            Body::Input {
                                text: text.clone(),
                                mode: protocol::Mode::Queue,
                                taken: true,
                                origin: None,
                            },
                        ))
                        .expect("context replacement encodes")
                    });
            }
            let projected = match context_fact(&e.body) {
                Some(ContextFact::ToolCall { .. }) => completed.contains(&e.seq),
                Some(ContextFact::Queued(_)) => taken.contains(&e.seq),
                Some(ContextFact::Edit) | None => false,
                Some(_) => true,
            };
            projected.then(|| protocol::Json::encode(e).expect("context entry encodes"))
        })
        .collect()
}

#[cfg(test)]
#[path = "transcript/context_image_tests.rs"]
mod context_image_tests;
