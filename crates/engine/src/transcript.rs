//! What a session's log says, read back for the model: the conversation
//! it sees.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::Arc;

use ext::host::Registry;
use llm::{Assistant, Message, Messages, ToolMessage, ToolResult};
use protocol::{Body, CallId, Decode, Entry, Outcome, Seq, Verdict};

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
        let edits = context_edits(entries.iter().map(Borrow::borrow));
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
            Body::Spawned { intent } => {
                #[derive(serde::Deserialize)]
                struct Image {
                    image: Vec<protocol::Json>,
                }
                if let Ok(image) = intent.parse::<Image>() {
                    let entries = image
                        .image
                        .iter()
                        .filter_map(|j| Entry::from_json(j.raw().as_bytes()).ok());
                    let messages = Self::of(entries, Arc::clone(&self.registry), "").into_context();
                    for message in messages.iter() {
                        self.messages.push(message.clone());
                    }
                    self.context = self.messages.len();
                    self.last = e.seq.get();
                }
                return;
            }
            Body::Input {
                text, taken: true, ..
            }
            | Body::Decision {
                d: Verdict::OnStop { text },
                ..
            } => Message::user(text.clone()),
            Body::Enqueue { envelope } => {
                self.waiting.insert(e.seq, envelope.payload.clone());
                return;
            }
            Body::Input { text, .. } => {
                self.waiting.insert(e.seq, text.clone());
                return;
            }
            Body::Take { input } => match self.waiting.remove(input) {
                Some(text) => Message::user(text),
                None => return,
            },
            Body::Assistant { text } => Message::assistant(text.clone()),
            Body::Context { text, .. } => {
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

    /// Pending exchanges cannot be replayed by a context-only child.
    pub fn into_context(self) -> Messages {
        let requested: std::collections::HashSet<_> = self
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a.tool_calls().map(|c| c.id.clone())),
                _ => None,
            })
            .flatten()
            .collect();
        let answered: std::collections::HashSet<_> = self
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Tool(t) => Some(t.results.iter().map(|r| r.id.clone())),
                _ => None,
            })
            .flatten()
            .collect();
        self.messages.iter().cloned().filter_map(|mut m| {
            match &mut m {
                Message::Assistant(a) => {
                    a.content.retain(|p| !matches!(p, llm::AssistantPart::ToolCall(c) if !c.provider_executed && !answered.contains(&c.id)));
                    if a.content.is_empty() { return None; }
                }
                Message::Tool(t) => {
                    t.results.retain(|r| requested.contains(&r.id));
                    if t.results.is_empty() { return None; }
                }
                _ => {},
            }
            Some(m)
        }).collect()
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

/// The image contains projection inputs, never an executable child ledger.
pub(crate) fn context_image(entries: &[Entry]) -> Vec<protocol::Json> {
    let completed: std::collections::HashSet<_> = entries
        .iter()
        .filter_map(|e| match e.body {
            Body::ToolResult { call, .. } => Some(call),
            _ => None,
        })
        .collect();
    let taken_inputs: std::collections::HashSet<_> = entries
        .iter()
        .filter_map(|e| match e.body {
            Body::Take { input } => Some(input),
            _ => None,
        })
        .collect();
    let edits = context_edits(entries);
    entries
        .iter()
        .filter_map(|e| {
            let projected = match e.body {
                Body::ToolCall { .. } => completed.contains(&CallId(e.seq)),
                Body::Input { taken, .. } => taken || taken_inputs.contains(&e.seq),
                Body::Enqueue { .. } => taken_inputs.contains(&e.seq),
                Body::Take { .. }
                | Body::Assistant { .. }
                | Body::Context { .. }
                | Body::Edit { .. }
                | Body::ModelResult { r#for: None, .. }
                | Body::ToolResult { .. }
                | Body::Fired { .. }
                | Body::Spawned { .. } => true,
                _ => false,
            };
            if projected {
                return Some(protocol::Json::encode(e).expect("context entry encodes"));
            }
            // An edit can begin at a ledger-only entry; retain its replacement's position.
            let edit = edits
                .iter()
                .rev()
                .find(|(from, to, _)| (*from..=*to).contains(&e.seq.get()));
            matches!(edit, Some((from, _, Some(_))) if *from == e.seq.get()).then(|| {
                protocol::Json::encode(&Entry::new(
                    e.seq,
                    e.epoch,
                    Body::Assistant {
                        text: String::new(),
                    },
                ))
                .expect("context position encodes")
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Epoch, Json, Mode, SessionId};
    fn entry(seq: u64, body: Body) -> Entry {
        Entry::new(Seq::new(seq).unwrap(), Epoch(0), body)
    }
    #[test]
    fn a_prefix_projects_edits_and_publications_without_executable_ownership() {
        let entries = vec![
            entry(
                1,
                Body::Input {
                    text: "old".into(),
                    mode: Mode::Queue,
                    taken: true,
                    origin: None,
                },
            ),
            entry(
                2,
                Body::Context {
                    text: "skill".into(),
                    publication: Some(protocol::publication::Publication {
                        namespace: "skills".into(),
                        key: Json::encode("review").unwrap(),
                    }),
                },
            ),
            entry(
                3,
                Body::Edit {
                    target: Seq::new(1).unwrap(),
                    upto: None,
                    replace: Some(Json::encode("effective").unwrap()),
                },
            ),
            entry(
                4,
                Body::ToolCall {
                    tool: "task".into(),
                    input: Json::null(),
                    id: Some("pending".into()),
                    card: None,
                    by: protocol::Audience::Model,
                },
            ),
            entry(
                5,
                Body::Watch {
                    child: SessionId::new(),
                },
            ),
            entry(
                6,
                Body::Edit {
                    target: Seq::new(5).unwrap(),
                    upto: None,
                    replace: Some(Json::encode("ownership note").unwrap()),
                },
            ),
        ];
        let image = context_image(&entries);
        let inherited: Vec<_> = image
            .iter()
            .map(|j| Entry::from_json(j.raw().as_bytes()).unwrap())
            .collect();
        assert!(
            inherited
                .iter()
                .all(|e| !matches!(e.body, Body::Watch { .. } | Body::ToolCall { .. }))
        );
        let registry = Arc::new(Registry::default());
        let expected = Transcript::of(&entries, registry.clone(), "").into_context();
        let intent = Json::encode(&serde_json::json!({"image": image})).unwrap();
        let child = [entry(1, Body::Spawned { intent })];
        let actual = Transcript::of(&child, registry, "").into_messages();
        assert_eq!(actual, expected);
        assert_eq!(
            actual,
            Messages::from_iter([
                Message::user("skill"),
                Message::user("effective"),
                Message::user("ownership note")
            ])
        );
        let ledger = protocol::ledger::Ledger::of(child.iter());
        assert!(ledger.open_calls().next().is_none());
        assert!(ledger.armed().next().is_none());
        assert!(
            ledger
                .publications()
                .contains("skills", &Json::encode("review").unwrap())
        );
    }
    #[test]
    fn a_prefix_never_exports_an_unmatched_provider_tool_exchange() {
        let call = |id: &str| {
            llm::AssistantPart::ToolCall(llm::ToolCall {
                id: id.into(),
                name: "task".into(),
                input: Json::encode(&serde_json::json!({})).unwrap(),
                invalid: None,
                provider_executed: false,
                meta: llm::Meta::new(),
            })
        };
        let assistant = Assistant {
            content: vec![call("complete"), call("pending")],
            origin: None,
        };
        let reply = llm::Response {
            message: assistant,
            finish: llm::Finish::new(llm::Reason::ToolCalls, None),
            usage: llm::Usage::default(),
            id: None,
            model: None,
            meta: llm::Meta::new(),
        };
        let entries = vec![
            entry(
                1,
                Body::ModelResult {
                    call: CallId::new(1).unwrap(),
                    outcome: Outcome::Done {
                        output: Json::encode(&reply).unwrap(),
                    },
                    r#for: None,
                    failure: None,
                },
            ),
            entry(
                2,
                Body::ToolCall {
                    tool: "task".into(),
                    input: Json::null(),
                    id: Some("complete".into()),
                    card: None,
                    by: protocol::Audience::Model,
                },
            ),
            entry(
                3,
                Body::ToolResult {
                    call: CallId::new(2).unwrap(),
                    outcome: Outcome::Done {
                        output: Json::encode("answer").unwrap(),
                    },
                    card: None,
                },
            ),
            entry(
                4,
                Body::ToolCall {
                    tool: "task".into(),
                    input: Json::null(),
                    id: Some("pending".into()),
                    card: None,
                    by: protocol::Audience::Model,
                },
            ),
        ];
        let image = context_image(&entries);
        let intent = Json::encode(&serde_json::json!({"image":image})).unwrap();
        let messages = Transcript::of(
            [entry(1, Body::Spawned { intent })],
            Arc::new(Registry::default()),
            "",
        )
        .into_messages();
        let Message::Assistant(assistant) = &messages[0] else {
            panic!("{messages:?}")
        };
        assert_eq!(
            assistant
                .tool_calls()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["complete"]
        );
        let Message::Tool(tool) = &messages[1] else {
            panic!("{messages:?}")
        };
        assert_eq!(
            tool.results
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["complete"]
        );
    }
}
