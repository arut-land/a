//! What a session's log says, read back for the model: the conversation
//! it sees.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ext::host::Registry;
use llm::{Assistant, Message, Messages, Tag, ToolMessage, ToolResult, tag};
use protocol::publication::{ContextImage, ContextInput, ContextLayer, ContextValue, RunnerKind};
use protocol::{Body, CallId, Entry, Envelope, Mode, Outcome, Segment, Segments, Seq, Verdict};

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
    waiting: HashMap<Seq, Waiting>,
    /// The seq of the last entry that changed the messages.
    last: u64,
    registry: Arc<Registry>,
}

/// Input waiting to be taken up.
enum Waiting {
    /// The user's, as it was given: rendered once it is known who takes it.
    Input(Segments, Mode),
    /// Another session's message, as it sent it: a model-only thread reads
    /// its payload as the turn it asks for.
    Inbox(Envelope),
}

/// `segments` as the model reads them: typed text as it is, and what the
/// user pasted or attached tagged as such; a steer, given to a turn already
/// running, says who gave it.
fn model_text(segments: &Segments, mode: Mode) -> String {
    let mut text = String::new();
    for segment in &segments.0 {
        match segment {
            Segment::Text(typed) => text.push_str(typed),
            Segment::Paste(pasted) => Tag::from(tag::Pasted(pasted)).render(&mut text),
            Segment::PasteRef(blob) => {
                let at = format!("blob:{}", blob.id.0);
                Tag::from(tag::PastedRef {
                    at: &at,
                    bytes: blob.size,
                })
                .render(&mut text);
            }
            Segment::Attachment(blob) => {
                let at = format!("blob:{}", blob.id.0);
                Tag::from(tag::Attached {
                    at: &at,
                    media_type: &blob.media_type,
                    bytes: blob.size,
                })
                .render(&mut text);
            }
        }
    }
    match mode {
        Mode::Queue => text,
        // Every steer comes from a client today; agents' and models' come
        // through the inbox.
        Mode::Steer => Tag::from(tag::Steer {
            from: "user",
            text: &text,
        })
        .to_string(),
    }
}

/// Another session's message, as the model reads it.
fn inbox_text(envelope: &Envelope) -> String {
    let from = envelope.id.sender.to_string();
    Tag::from(tag::Inbox {
        from: &from,
        channel: &envelope.id.purpose,
        text: &envelope.payload,
    })
    .to_string()
}

fn context_fact(body: &Body) -> Option<ContextValue> {
    Some(match body {
        Body::Input {
            segments,
            taken: true,
            ..
        } => ContextValue::User(model_text(segments, Mode::Queue)),
        Body::Decision {
            d: Verdict::OnStop { text },
            ..
        } => ContextValue::User(text.clone()),
        Body::Input { segments, mode, .. } => ContextValue::Queued(segments.clone(), *mode),
        Body::Enqueue { envelope } => ContextValue::Inbox(envelope.clone()),
        Body::Take { input } => ContextValue::Taken(*input),
        Body::Assistant { text } => ContextValue::Assistant(text.clone()),
        Body::Context { text, publication } => {
            ContextValue::Context(text.clone(), publication.clone())
        }
        Body::Fired { outcome, .. } => ContextValue::Child(outcome.clone()),
        Body::ModelResult {
            r#for: None,
            outcome: Outcome::Done { output },
            ..
        } => ContextValue::Reply(output.clone()),
        Body::ToolCall {
            tool, id: Some(id), ..
        } => ContextValue::ToolCall(id.clone(), tool.clone()),
        Body::ToolResult { call, outcome, .. } => ContextValue::ToolResult(*call, outcome.clone()),
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
            ContextValue::Queued(segments, mode) => {
                self.waiting.insert(seq, Waiting::Input(segments, mode));
                return;
            }
            ContextValue::Inbox(envelope) => {
                self.waiting.insert(seq, Waiting::Inbox(envelope));
                return;
            }
            ContextValue::Taken(input) => match self.waiting.remove(&input) {
                Some(waiting) if self.model_only => {
                    // As the side-thread runner reads it: what was given,
                    // untagged.
                    let text = match waiting {
                        Waiting::Input(segments, _) => segments.text(),
                        Waiting::Inbox(envelope) => envelope.payload,
                    };
                    if let Ok((turn, _)) = ext::ModelTurn::from_intent(&text) {
                        for message in turn.messages.iter() {
                            self.messages.push(message.clone());
                        }
                        self.last = seq.get();
                        return;
                    }
                    Message::user(text)
                }
                Some(Waiting::Input(segments, mode)) => Message::user(model_text(&segments, mode)),
                Some(Waiting::Inbox(envelope)) => Message::user(inbox_text(&envelope)),
                None => return,
            },
            ContextValue::Assistant(text) => Message::assistant(text),
            ContextValue::Context(text, _) => {
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
            ContextValue::ToolCall(id, tool) => {
                self.calls.insert(CallId(seq), (id, tool));
                return;
            }
            ContextValue::ToolResult(call, outcome) => {
                let Some((id, tool)) = self.calls.remove(&call) else {
                    return;
                };
                let ext::Presented { output, error } =
                    self.registry.present_result(&tool, &outcome);
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
        let mut answered: HashMap<usize, HashSet<_>> = HashMap::new();
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

/// Edits stay in their owning layer; only inherited anchors are rebased.
pub(crate) fn context_image(entries: &[Entry]) -> ContextImage {
    normalize(&entries.iter().collect::<Vec<_>>(), true)
}

fn normalize(entries: &[&Entry], inherited: bool) -> ContextImage {
    let edits = context_edits(entries.iter().copied());
    let completed = inherited.then(|| {
        entries
            .iter()
            .filter_map(|e| match &e.body {
                Body::ToolResult { call, .. } => Some(call.0),
                Body::Take { input } => Some(*input),
                _ => None,
            })
            .collect::<HashSet<_>>()
    });
    let mut image = vec![ContextLayer::default()];
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
            let Ok(mut spawn) = protocol::publication::SpawnImage::from_json(intent) else {
                continue;
            };
            image[0].0 = spawn.runner;
            let offset = image.len();
            if spawn.image.is_empty() {
                image.push(ContextLayer::default());
            } else {
                for ContextLayer(_, inputs) in &mut spawn.image {
                    for ContextInput(_, value) in inputs {
                        if let ContextValue::Inherited(scope) = value {
                            *scope += offset;
                        }
                    }
                }
                image.extend(spawn.image);
            }
            ContextValue::Inherited(offset)
        } else {
            let Some(value) = context_fact(&e.body) else {
                continue;
            };
            if completed.as_ref().is_some_and(|done| {
                matches!(
                    value,
                    ContextValue::ToolCall(..) | ContextValue::Queued(..) | ContextValue::Inbox(_)
                ) && !done.contains(&e.seq)
            }) {
                continue;
            }
            value
        };
        image[0].1.push(ContextInput(e.seq, value));
    }
    image
}

fn project(image: ContextImage, registry: Arc<Registry>, system: &str) -> Transcript {
    let mut inherited = HashMap::new();
    for (scope, ContextLayer(runner, inputs)) in image.into_iter().enumerate().rev() {
        let mut transcript =
            Transcript::empty(Arc::clone(&registry), if scope == 0 { system } else { "" });
        transcript.model_only = runner == RunnerKind::ModelOnly;
        for ContextInput(seq, value) in inputs {
            if let ContextValue::Inherited(child) = value {
                transcript.inherit(
                    &inherited.remove(&child).expect("inherited scope reduced"),
                    seq,
                );
            } else {
                transcript.apply_input(seq, value, None);
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

#[cfg(test)]
mod tests {
    use super::*;
    use ext::point::{PresentResult, Presenting};
    use ext::{Extension, Presented, Registrar, Rewrite};
    use protocol::{BlobId, BlobRef, EnvelopeId, Epoch, Json, SessionId};

    fn entry(seq: u64, body: Body) -> Entry {
        Entry::new(Seq::new(seq).unwrap(), Epoch(0), body)
    }

    fn input(segments: impl Into<Segments>, mode: Mode, taken: bool) -> Body {
        Body::Input {
            segments: segments.into(),
            mode,
            taken,
            origin: None,
        }
    }

    fn take(input: u64) -> Body {
        Body::Take {
            input: Seq::new(input).unwrap(),
        }
    }

    fn seen(entries: &[Entry], registry: Registry) -> Vec<Message> {
        Transcript::of(entries, Arc::new(registry), "")
            .into_messages()
            .to_vec()
    }

    fn said(messages: &[Message]) -> Vec<String> {
        (messages.iter())
            .filter_map(|message| match message {
                Message::User(user) => match &user.content[..] {
                    [llm::UserPart::Text(text)] => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_paste_reaches_the_model_tagged_and_typed_text_as_it_is() {
        let blob = BlobRef {
            id: BlobId("ab12".into()),
            size: 9000,
            media_type: "text/plain".into(),
        };
        let segments = vec![
            Segment::Text("see ".into()),
            Segment::Paste("a\nb".into()),
            Segment::PasteRef(blob),
        ];
        let entries = [entry(1, input(segments, Mode::Queue, true))];
        assert_eq!(
            said(&seen(&entries, Registry::default())),
            [
                "see <pasted-content lines=\"2\" bytes=\"3\">\na\nb\n</pasted-content>\
              <pasted-content path=\"blob:ab12\" bytes=\"9000\"/>"
            ]
        );
    }

    #[test]
    fn a_steer_reaches_the_model_naming_who_gave_it() {
        let entries = [
            entry(1, input("first", Mode::Queue, true)),
            entry(2, input("not that", Mode::Steer, false)),
            entry(3, take(2)),
            entry(4, input("later", Mode::Queue, false)),
            entry(5, take(4)),
        ];
        assert_eq!(
            said(&seen(&entries, Registry::default())),
            [
                "first",
                "<steer from=\"user\">\nnot that\n</steer>",
                "later"
            ]
        );
    }

    #[test]
    fn another_sessions_message_names_its_sender_and_channel() {
        let sender = SessionId::new();
        let envelope = Envelope {
            id: EnvelopeId {
                sender,
                call: CallId::new(3).unwrap(),
                purpose: "subagent:message/0".into(),
            },
            destination: SessionId::new(),
            payload: "hello".into(),
        };
        let entries = [entry(1, Body::Enqueue { envelope }), entry(2, take(1))];
        assert_eq!(
            said(&seen(&entries, Registry::default())),
            [format!(
                "<message from=\"{sender}\" channel=\"subagent:message/0\">\nhello\n</message>"
            )]
        );
    }

    #[test]
    fn a_model_only_thread_reads_waiting_input_as_given_and_an_agent_reads_it_tagged() {
        let pasted = Segments(vec![
            Segment::Text("see ".into()),
            Segment::Paste("x".into()),
        ]);
        let inputs = |runner| {
            let seq = |n| Seq::new(n).unwrap();
            vec![ContextLayer(
                runner,
                vec![
                    ContextInput(seq(1), ContextValue::Queued(pasted.clone(), Mode::Steer)),
                    ContextInput(seq(2), ContextValue::Taken(seq(1))),
                ],
            )]
        };
        let read = |runner| {
            let transcript = project(inputs(runner), Arc::new(Registry::default()), "");
            said(&transcript.into_messages().to_vec())
        };
        assert_eq!(read(RunnerKind::ModelOnly), ["see x"]);
        assert_eq!(
            read(RunnerKind::Agent),
            [
                "<steer from=\"user\">\nsee <pasted-content lines=\"1\" bytes=\"1\">\nx\n</pasted-content>\n</steer>"
            ]
        );
    }

    struct Shown;

    impl Extension for Shown {
        const NAME: &'static str = "shown";
        const VERSION: u32 = 1;

        fn contribute(&self, r: &mut Registrar) {
            r.on::<PresentResult>(|(), ended: Presenting<'_>| {
                Rewrite::Replace(Presented {
                    output: llm::Output::Text(format!(
                        "{} said {:?}",
                        ended.tool, ended.presented.output
                    )),
                    error: ended.presented.error,
                })
            });
        }
    }

    #[test]
    fn present_result_decides_what_the_model_sees_of_an_outcome_and_defaults_to_the_tool() {
        let entries = [
            entry(
                1,
                Body::ToolCall {
                    invoke: None,
                    tool: "t".into(),
                    input: Json::null(),
                    id: Some("c1".into()),
                    card: None,
                    by: protocol::Audience::Model,
                },
            ),
            entry(
                2,
                Body::ToolResult {
                    call: CallId::new(1).unwrap(),
                    outcome: Outcome::Done {
                        output: Json::encode("ok").unwrap(),
                    },
                    card: None,
                },
            ),
        ];
        let output = |registry| match seen(&entries, registry).last() {
            Some(Message::Tool(tool)) => tool.results[0].output.clone(),
            other => panic!("{other:?}"),
        };
        let plain = output(Registry::default());
        assert_eq!(plain, llm::Output::Json(Json::encode("ok").unwrap()));
        let shown = output(Registry::default().with(&Shown).unwrap());
        assert_eq!(shown, llm::Output::Text(format!("t said {plain:?}")));
    }
}
