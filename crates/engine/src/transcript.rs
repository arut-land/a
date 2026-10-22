//! What a session's log says, read back for the model: the conversation
//! it sees.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use ext::host::Registry;
use llm::{Assistant, Message, Messages, ToolMessage, ToolResult};
use protocol::publication::{ContextImage, ContextInput, ContextValue, RunnerKind};
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

fn context_fact(body: &Body) -> Option<ContextValue> {
    Some(match body {
        Body::Input {
            text, taken: true, ..
        }
        | Body::Decision {
            d: Verdict::OnStop { text },
            ..
        } => ContextValue::User(text.clone()),
        Body::Input { text, .. } => ContextValue::Queued(text.clone()),
        Body::Enqueue { envelope } => ContextValue::Queued(envelope.payload.clone()),
        Body::Take { input } => ContextValue::Taken(*input),
        Body::Assistant { text } => ContextValue::Assistant(text.clone()),
        Body::Context { text, publication } => ContextValue::Context {
            text: text.clone(),
            publication: publication.clone(),
        },
        Body::Fired { outcome, .. } => ContextValue::Child(outcome.clone()),
        Body::ModelResult {
            r#for: None,
            outcome: Outcome::Done { output },
            ..
        } => ContextValue::Reply(output.clone()),
        Body::ToolCall {
            tool, id: Some(id), ..
        } => ContextValue::ToolCall {
            id: id.clone(),
            tool: tool.clone(),
        },
        Body::ToolResult { call, outcome, .. } => ContextValue::ToolResult {
            call: *call,
            outcome: outcome.clone(),
        },
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
        project(
            normalize(
                &entries.iter().map(Borrow::borrow).collect::<Vec<_>>(),
                false,
            ),
            registry,
            system,
        )
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
        if let Body::Spawned { intent } = &e.body {
            if let Ok(image) = protocol::publication::SpawnImage::from_json(intent) {
                self.model_only = image.runner == RunnerKind::ModelOnly;
                let messages = project(image.image, Arc::clone(&self.registry), "").into_context();
                self.inherit(&messages, e.seq);
            }
        } else if let Some(value) = context_fact(&e.body) {
            self.apply_input(e.seq, value, fresh);
        }
    }

    fn inherit(&mut self, messages: &Messages, seq: Seq) {
        for message in messages.iter() {
            self.messages.push(message.clone());
        }
        self.context = self.messages.len();
        self.last = seq.get();
    }

    fn apply_input(&mut self, seq: Seq, value: ContextValue, fresh: Option<Assistant>) {
        let message = match value {
            ContextValue::User(text) => Message::user(text),
            ContextValue::Queued(text) => {
                self.waiting.insert(seq, text);
                return;
            }
            ContextValue::Taken(input) => match self.waiting.remove(&input) {
                Some(text) if self.model_only => {
                    if let Ok((turn, _)) = ext::ModelTurn::from_intent(&text) {
                        for message in turn.messages.iter() {
                            self.messages.push(message.clone());
                        }
                        self.last = seq.get();
                        return;
                    }
                    Message::user(text)
                }
                Some(text) => Message::user(text),
                None => return,
            },
            ContextValue::Assistant(text) => Message::assistant(text),
            ContextValue::Context { text, .. } => {
                self.messages.insert(self.context, Message::user(text));
                self.context += 1;
                self.last = seq.get();
                return;
            }
            ContextValue::Child(outcome) => Message::user(ext::child_result(&outcome)),
            ContextValue::Reply(output) => {
                let reply = match fresh {
                    Some(message) => Ok(message),
                    None => llm::Response::from_json(output.raw()).map(|r| r.message),
                };
                match reply {
                    Ok(message) => Message::Assistant(message),
                    Err(_) => return,
                }
            }
            ContextValue::ToolCall { id, tool } => {
                self.calls.insert(CallId(seq), (id, tool));
                return;
            }
            ContextValue::ToolResult { call, outcome } => {
                let Some((id, tool)) = self.calls.remove(&call) else {
                    return;
                };
                let (output, error) = self.registry.tools().to_model(&tool, &outcome);
                let result = ToolResult {
                    id: id.into(),
                    name: tool.into(),
                    output,
                    error,
                    meta: llm::Meta::new(),
                };
                self.last = seq.get();
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
            ContextValue::Inherited(_) => return,
        };
        self.messages.push(message);
        self.last = seq.get();
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

/// Edits are resolved in their owning scope before inherited inputs are rebased.
pub(crate) fn context_image(entries: &[Entry]) -> ContextImage {
    normalize(&entries.iter().collect::<Vec<_>>(), true)
}

fn normalize(entries: &[&Entry], inherited: bool) -> ContextImage {
    let edits = context_edits(entries.iter().copied());
    let completed: std::collections::HashSet<_> = entries
        .iter()
        .take(if inherited { entries.len() } else { 0 })
        .filter_map(|e| match &e.body {
            Body::ToolResult { call, .. } => Some(call.0),
            Body::Take { input } => Some(*input),
            _ => None,
        })
        .collect();
    let mut image = ContextImage {
        scopes: vec![RunnerKind::Agent],
        inputs: Vec::new(),
    };
    for e in entries {
        let value = if let Some((from, _, text)) = edits
            .iter()
            .rev()
            .find(|(from, to, _)| (*from..=*to).contains(&e.seq.get()))
        {
            if *from != e.seq.get() {
                continue;
            }
            let Some(text) = text else {
                continue;
            };
            ContextValue::User(text.clone())
        } else if let Body::Spawned { intent } = &e.body {
            let Ok(spawn) = protocol::publication::SpawnImage::from_json(intent) else {
                continue;
            };
            image.scopes[0] = spawn.runner;
            let offset = image.scopes.len();
            if spawn.image.scopes.is_empty() {
                image.scopes.push(RunnerKind::Agent);
            } else {
                image.scopes.extend(spawn.image.scopes);
            }
            image
                .inputs
                .extend(spawn.image.inputs.into_iter().map(|mut input| {
                    input.scope += offset;
                    if let ContextValue::Inherited(scope) = &mut input.value {
                        *scope += offset;
                    }
                    input
                }));
            ContextValue::Inherited(offset)
        } else {
            let Some(value) = context_fact(&e.body) else {
                continue;
            };
            match &value {
                ContextValue::ToolCall { .. } if inherited && !completed.contains(&e.seq) => {
                    continue;
                }
                ContextValue::Queued(_) if inherited && !completed.contains(&e.seq) => continue,
                _ => {}
            }
            value
        };
        image.inputs.push(ContextInput {
            scope: 0,
            seq: e.seq,
            value,
        });
    }
    image
}

fn project(image: ContextImage, registry: Arc<Registry>, system: &str) -> Transcript {
    let mut layers: Vec<_> = image.scopes.iter().map(|_| Vec::new()).collect();
    for input in image.inputs {
        layers[input.scope].push(input);
    }
    let mut inherited = HashMap::new();
    for (scope, inputs) in layers.into_iter().enumerate().rev() {
        let mut transcript =
            Transcript::empty(Arc::clone(&registry), if scope == 0 { system } else { "" });
        transcript.model_only = image.scopes[scope] == RunnerKind::ModelOnly;
        for input in inputs {
            if let ContextValue::Inherited(child) = input.value {
                transcript.inherit(
                    &inherited.remove(&child).expect("inherited scope reduced"),
                    input.seq,
                );
            } else {
                transcript.apply_input(input.seq, input.value, None);
            }
        }
        if scope == 0 {
            return transcript;
        }
        inherited.insert(scope, transcript.into_context());
    }
    Transcript::empty(registry, system)
}

#[cfg(test)]
#[path = "transcript/context_image_tests.rs"]
mod context_image_tests;
