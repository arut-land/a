//! What a session's log says, read back for the model: the conversation
//! it sees.

#[cfg(test)]
use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::sync::Arc;

use ext::blob::Places;
use ext::host::Registry;
use llm::tag::Part;
use llm::{Assistant, Message, Messages, Tag, ToolMessage, ToolResult, tag};
use protocol::publication::{ContextImage, ContextInput, ContextLayer, ContextValue, RunnerKind};
use protocol::{
    BlobRef, Body, CallId, Carried, Entry, EnvelopeId, Held, Mode, Outcome, OutputStream, Payload,
    Segment, Segments, Seq, ToolOutcome, Verdict,
};

/// Provider replies remain intact; live updates and recovery share projection rules.
pub struct Transcript {
    messages: Messages,
    resident: llm::Resident,
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
    /// What is queued and not taken up, by seq: the model reads it once
    /// it is, where it is taken.
    waiting: HashMap<Seq, Waiting>,
    /// Tools awaiting their final detached or job completion.
    tools: HashMap<CallId, String>,
    /// Calls that went on in the background: the result logged for one
    /// is a placeholder, and its outcome comes queued.
    detached: HashSet<CallId>,
    /// Process groups can outlive a detached call's own completion.
    jobs: HashSet<CallId>,
    /// The seq of the last entry that changed the messages.
    last: u64,
    /// The seq of the last entry folded: the transcript goes on from the
    /// entry after it.
    through: u64,
    registry: Arc<Registry>,
    /// How references are named for the model.
    places: Places,
    /// Streams awaiting their final detached or job completion.
    streams: HashMap<CallId, OutputStream>,
}

/// Input waiting to be taken up.
enum Waiting {
    /// The user's, as it was given: rendered once it is known who takes it.
    Input(Segments, Mode),
    /// Another session's message, as it sent it: a model-only thread reads
    /// its text as the turn it asks for.
    Inbox(EnvelopeId, String),
    /// What the session's own envelope brought back, as the model reads it.
    Own(String),
}

/// `segments` as the model reads them: typed text as it is, and what the
/// user pasted or attached tagged as such; a steer, given to a turn already
/// running, says who gave it.
fn model_text(segments: &Segments, mode: Mode, places: &Places) -> String {
    let parts = segments.0.iter().map(|segment| match segment {
        Segment::Text(typed) => Part::Text(typed.into()),
        Segment::Paste(pasted) => Tag::from(tag::Pasted(pasted)).into(),
        Segment::PasteRef(blob) => Tag::from(tag::PastedRef {
            at: places.blob(blob).map(Into::into),
            bytes: blob.size,
        })
        .into(),
        Segment::Attachment(blob) => Tag::from(tag::Attached {
            at: places.blob(blob).map(Into::into),
            media_type: &blob.media_type,
            bytes: blob.size,
        })
        .into(),
    });
    match mode {
        // Typed text outside any tag is the user's own, so it goes as typed.
        Mode::Queue => {
            let mut text = String::new();
            for part in parts {
                match part {
                    Part::Text(typed) | Part::Markup(typed) => text.push_str(&typed),
                    Part::Tag(tag) => tag.render(&mut text),
                }
            }
            text
        }
        // Every steer comes from a client today; agents' and models' come
        // through the inbox.
        Mode::Steer => Tag::from(tag::Steer {
            from: "user",
            body: parts.collect(),
        })
        .to_string(),
    }
}

/// Another session's message, as the model reads it.
fn inbox_text(id: &EnvelopeId, text: &str) -> String {
    let from = id.sender.to_string();
    Tag::from(tag::Inbox {
        from: &from,
        channel: &id.purpose,
        text,
    })
    .to_string()
}

fn context_fact(body: &Body) -> Option<ContextValue> {
    Some(match body {
        Body::Input {
            segments,
            taken: true,
            ..
        // Published to other sessions: named by tree place, not this
        // session's paths.
        } => ContextValue::User(model_text(segments, Mode::Queue, &Places::default())),
        Body::Decision {
            d: Verdict::OnStop { text },
            ..
        } => ContextValue::User(text.clone()),
        Body::Input { segments, mode, .. } => ContextValue::Queued(segments.clone(), *mode),
        Body::Enqueue { envelope } => {
            let (call, held) = (envelope.id().call, envelope.held.clone());
            match envelope.payload() {
                Carried::Message(text) => ContextValue::Inbox(envelope.id().clone(), text.clone()),
                Carried::Job(text) => ContextValue::Job(call, text.clone(), held),
                Carried::Child(outcome) => ContextValue::Child(outcome.clone()),
                Carried::Detached(outcome) => ContextValue::Background(call, outcome.clone(), held),
            }
        }
        Body::Take { input, into: None } => ContextValue::Taken(*input),
        Body::Take {
            input,
            into: Some(call),
        } => ContextValue::Into(*input, *call),
        Body::Withdraw { input, .. } => ContextValue::Withdrawn(*input),
        Body::Detached { call } => ContextValue::Detached(*call),
        Body::Assistant { text } => ContextValue::Assistant(text.clone()),
        Body::Context { text, publication } => {
            ContextValue::Context(text.clone(), publication.clone())
        }
        Body::ModelResult {
            r#for: None,
            outcome: Outcome::Done { output },
            ..
        } => ContextValue::Reply(output.clone()),
        Body::ToolCall {
            tool, id: Some(id), ..
        } => ContextValue::ToolCall(id.clone(), tool.clone()),
        Body::ToolResult {
            call,
            outcome,
            held,
            ..
        } => ContextValue::ToolResult(*call, outcome.clone(), held.clone()),
        _ => return None,
    })
}

impl Waiting {
    /// As the model reads it, taken by an agent.
    fn text(self, places: &Places) -> String {
        match self {
            Self::Input(segments, mode) => model_text(&segments, mode, places),
            Self::Inbox(id, text) => inbox_text(&id, &text),
            Self::Own(text) => text,
        }
    }
}

/// The image types a result's blob is shown to the model as: those every
/// provider reads. Another is only named.
const SHOWN: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];

/// `output`, telling the model where it reads what the result holds, each
/// reference left out when it cannot be read; and showing it each image the
/// result holds, read back by `bytes`, after what it already shows.
fn with_held(
    output: llm::Output,
    held: &[ext::Placed],
    bytes: impl Fn(&BlobRef) -> Option<Arc<[u8]>>,
) -> llm::Output {
    let image = |placed: &ext::Placed| match &placed.held {
        Held::Blob(blob) if SHOWN.contains(&blob.media_type.as_str()) => Some(blob.clone()),
        Held::Blob(_) | Held::Stream(_) => None,
    };
    let lines: Vec<String> = (held.iter())
        .filter_map(|placed| {
            let at = placed.at.as_deref()?;
            Some(match &placed.held {
                Held::Blob(_) if image(placed).is_some() => format!("[image: {at}]"),
                Held::Blob(_) => format!("[full output: {at}]"),
                Held::Stream(_) => format!("[output, as it grows: {at}]"),
            })
        })
        .collect();
    let images: Vec<llm::UserPart> = (held.iter().filter_map(image))
        .filter_map(|blob| {
            Some(llm::UserPart::Media(llm::Media {
                source: llm::Source::Bytes {
                    data: bytes(&blob)?,
                },
                media_type: blob.media_type.into(),
                meta: llm::Meta::new(),
            }))
        })
        .collect();
    if lines.is_empty() && images.is_empty() {
        return output;
    }
    let lines = lines.join("\n");
    let mut parts = match output {
        // What the presentation shows already, media included, stays.
        llm::Output::Content(mut parts) => {
            if !lines.is_empty() {
                parts.push(llm::UserPart::Text(llm::Text::new(lines)));
            }
            parts
        }
        output => {
            let mut text = text_of(output);
            if !text.is_empty() && !lines.is_empty() {
                text.push('\n');
            }
            text.push_str(&lines);
            if images.is_empty() {
                return llm::Output::Text(text);
            }
            let text = (!text.is_empty()).then(|| llm::UserPart::Text(llm::Text::new(text)));
            text.into_iter().collect()
        }
    };
    parts.extend(images);
    llm::Output::Content(parts)
}

/// An output as text, for a message of text alone.
fn text_of(output: llm::Output) -> String {
    match output {
        llm::Output::Text(text) => text,
        llm::Output::Json(json) => json.raw().to_owned(),
        llm::Output::Content(parts) => (parts.into_iter())
            .filter_map(|part| match part {
                llm::UserPart::Text(text) => Some(text.text),
                llm::UserPart::Media(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

impl Transcript {
    /// As [`Transcript::placed`], with no store, as tests project: a
    /// payload a blob holds is not read.
    #[cfg(test)]
    pub fn of(
        entries: impl IntoIterator<Item = impl Borrow<Entry>>,
        registry: Arc<Registry>,
        system: &str,
    ) -> Self {
        let entries: Vec<_> = entries.into_iter().collect();
        Self::replay(
            |each| {
                for entry in &entries {
                    each(entry.borrow());
                }
                Ok(())
            },
            registry,
            system,
            &Places::default(),
        )
        .expect("in-memory transcript replay cannot fail")
    }

    /// The transcript of `entries`, on `registry`, that `system` prompts,
    /// as their edits leave it: the entries an `edit` covers stand as its
    /// text, a user's message, or are left out; the latest edit of an
    /// entry is what stands. References are named, and payloads held as
    /// blobs read, through `places`.
    pub fn placed(
        entries: &[Entry],
        registry: Arc<Registry>,
        system: &str,
        places: &Places,
    ) -> Self {
        Self::replay(
            |each| {
                for entry in entries {
                    each(entry);
                }
                Ok(())
            },
            registry,
            system,
            places,
        )
        .expect("in-memory transcript replay cannot fail")
    }

    /// Replays a stable history twice, retaining edits and background call identities between passes.
    pub(super) fn replay(
        mut entries: impl FnMut(&mut dyn FnMut(&Entry)) -> io::Result<()>,
        registry: Arc<Registry>,
        system: &str,
        places: &Places,
    ) -> io::Result<Self> {
        let mut edits = Vec::new();
        let mut background = HashSet::new();
        let mut jobs = HashSet::new();
        entries(&mut |entry| {
            if let Some(edit) = context_edit(entry) {
                edits.push(edit);
            }
            match &entry.body {
                Body::ToolProcess {
                    call,
                    background: true,
                    ..
                } => {
                    background.insert(*call);
                }
                Body::Enqueue { envelope }
                    if matches!(envelope.payload(), Carried::Job(_) | Carried::Detached(_)) =>
                {
                    background.insert(envelope.id().call);
                    if matches!(envelope.payload(), Carried::Job(_)) {
                        jobs.insert(envelope.id().call);
                    }
                }
                _ => {}
            }
        })?;
        let edits = Edits::new(edits);
        let mut transcript = Self::empty(registry, system, places);
        entries(&mut |entry| {
            transcript.through = entry.seq.get();
            match edits.at(entry.seq) {
                Some((from, Some(text))) if from == entry.seq.get() => {
                    transcript.apply_input(entry.seq, ContextValue::User(text.clone()), None);
                }
                Some(_) => {}
                None => {
                    if let Body::ToolCall {
                        tool, id: Some(_), ..
                    } = &entry.body
                        && background.contains(&CallId(entry.seq))
                    {
                        transcript.tools.insert(CallId(entry.seq), tool.clone());
                        if jobs.contains(&CallId(entry.seq)) {
                            transcript.jobs.insert(CallId(entry.seq));
                        }
                    }
                    transcript.apply(entry, None);
                }
            }
        })?;
        Ok(transcript)
    }

    fn empty(registry: Arc<Registry>, system: &str, places: &Places) -> Self {
        let messages = Messages::from_iter((!system.is_empty()).then(|| Message::system(system)));
        Self {
            context: messages.len(),
            messages,
            resident: llm::Resident::default(),
            model_only: false,
            calls: HashMap::new(),
            answered: Vec::new(),
            waiting: HashMap::new(),
            tools: HashMap::new(),
            detached: HashSet::new(),
            jobs: HashSet::new(),
            last: 0,
            through: 0,
            registry,
            places: places.clone(),
            streams: HashMap::new(),
        }
    }

    /// Folds entry `e`. `fresh` is the message of the reply this daemon
    /// just had, which `e` stores: taken as it is rather than read again.
    pub fn apply(&mut self, e: &Entry, fresh: Option<Assistant>) -> bool {
        self.retain();
        if !self.accepts(e) {
            return false;
        }
        self.through = e.seq.get();
        if let Body::Spawned { intent } = &e.body {
            if let Ok(image) = protocol::publication::SpawnImage::from_json(intent) {
                self.model_only = image.runner == RunnerKind::ModelOnly;
                let messages = project(image.image, Arc::clone(&self.registry), "", &self.places)
                    .into_context();
                self.inherit(&messages, e.seq);
            }
        } else if let Body::ToolProcess {
            call,
            background: true,
            ..
        } = &e.body
        {
            self.jobs.insert(*call);
            if let Some((_, tool)) = self.calls.get(call) {
                self.tools.insert(*call, tool.clone());
            }
        } else if let Some(value) = context_fact(&e.body) {
            self.apply_input(e.seq, value, fresh);
        }
        true
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
            ContextValue::Inbox(id, text) => {
                self.waiting.insert(seq, Waiting::Inbox(id, text));
                return;
            }
            ContextValue::Taken(input) => match self.waiting.remove(&input) {
                Some(waiting) if self.model_only => {
                    // As the side-thread runner reads it: what was given,
                    // untagged.
                    let text = match waiting {
                        Waiting::Input(segments, _) => segments.text(),
                        Waiting::Inbox(_, text) | Waiting::Own(text) => text,
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
                Some(waiting) => Message::user(waiting.text(&self.places)),
                None => return,
            },
            ContextValue::Into(input, call) => match self.waiting.remove(&input) {
                Some(waiting) => match self.join_result(call, waiting.text(&self.places)) {
                    Ok(()) => {
                        self.last = seq.get();
                        return;
                    }
                    Err(text) => Message::user(text),
                },
                None => return,
            },
            value @ (ContextValue::Withdrawn(_)
            | ContextValue::Child(_)
            | ContextValue::Background(..)
            | ContextValue::Job(..)
            | ContextValue::Detached(_)) => return self.queue(seq, value),
            ContextValue::Assistant(text) => Message::assistant(text),
            ContextValue::Context(text, _) => {
                self.messages.insert(self.context, Message::user(text));
                self.context += 1;
                self.last = seq.get();
                return;
            }
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
            ContextValue::ToolResult(call, outcome, held) => {
                return self.answer(seq, call, &outcome, &held);
            }
            ContextValue::Inherited(_) => return,
        };
        self.messages.push(message);
        self.last = seq.get();
    }

    /// The result of call `call` the model made, in the tool message its
    /// reply's results share, in the calls' order: a detached call's is a
    /// placeholder, holding its stream.
    fn answer(&mut self, seq: Seq, call: CallId, outcome: &ToolOutcome, held: &[Held]) {
        let Some((id, tool)) = self.calls.remove(&call) else {
            return;
        };
        for held in held {
            if let Held::Stream(stream) = held {
                self.streams.insert(call, *stream);
            }
        }
        if self.detached.contains(&call) || self.streams.contains_key(&call) {
            self.tools.insert(call, tool.clone());
        }
        let ext::Presented { output, error } = if self.detached.contains(&call) {
            let text = "[it goes on in the background; its result follows once it ends]";
            ext::Presented {
                output: with_held(
                    llm::Output::Text(text.to_owned()),
                    &self.name_held(held),
                    |blob| self.places.bytes(blob),
                ),
                error: false,
            }
        } else {
            self.present(&tool, outcome, held)
        };
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
    }

    /// Folds what the queue holds of the session's own: an item withdrawn,
    /// a result that came back, a call gone to the background.
    fn queue(&mut self, seq: Seq, value: ContextValue) {
        let text = match value {
            ContextValue::Withdrawn(input) => {
                self.waiting.remove(&input);
                return;
            }
            ContextValue::Detached(call) => {
                self.detached.insert(call);
                return;
            }
            ContextValue::Child(outcome) => {
                // Read back as a tool's result is, where a blob holds it.
                let shown = match &outcome {
                    Outcome::Done {
                        output: Payload::Blob(_),
                    } => Some(text_of(self.present("", &outcome, &[]).output)),
                    _ => None,
                };
                ext::child_result(&outcome, shown.as_deref())
            }
            ContextValue::Background(call, outcome, held) => self.background(call, &outcome, &held),
            ContextValue::Job(call, text, held) => {
                self.jobs.remove(&call);
                // Its stream, sealed as the group ended, is read as a blob.
                let log = (self.name_held(&held).into_iter()).find_map(|placed| placed.at);
                self.ended(call, None, &text, log)
            }
            _ => return,
        };
        self.waiting.insert(seq, Waiting::Own(text));
    }

    /// What the model reads of the outcome of call `call`, which went on
    /// in the background.
    fn background(&mut self, call: CallId, outcome: &ToolOutcome, held: &[Held]) -> String {
        self.detached.remove(&call);
        let tool = self.tools.get(&call).cloned().unwrap_or_default();
        let ext::Presented { output, .. } = self.present(&tool, outcome, held);
        self.ended(call, Some(&text_of(output)), "", None)
    }

    /// What the model is shown of a call of `tool` that ended in
    /// `outcome`, holding `held`: each reference named, and a payload a
    /// blob holds read back, before the presentation is asked.
    fn present(&self, tool: &str, outcome: &ToolOutcome, held: &[Held]) -> ext::Presented {
        let held = self.name_held(held);
        let stored = match outcome {
            Outcome::Done {
                output: Payload::Blob(blob),
            } => Some(ext::Stored {
                at: self.places.blob(blob),
                text: self.places.read(blob),
            }),
            _ => None,
        };
        let presented = self.registry.present_result(tool, outcome, &held, stored);
        ext::Presented {
            output: with_held(presented.output, &held, |blob| self.places.bytes(blob)),
            ..presented
        }
    }

    /// What a result holds, each reference named through the session's
    /// places.
    fn name_held(&self, held: &[Held]) -> Vec<ext::Placed> {
        (held.iter())
            .map(|held| ext::Placed {
                at: match held {
                    Held::Blob(blob) => self.places.blob(blob),
                    Held::Stream(stream) => Some(self.places.stream(*stream)),
                },
                held: held.clone(),
            })
            .collect()
    }

    /// A call that went on in the background ended, as the model reads it:
    /// with its output, or with `note` about its end; its whole output at
    /// `log`, else where it streamed.
    fn ended(
        &mut self,
        call: CallId,
        output: Option<&str>,
        note: &str,
        log: Option<String>,
    ) -> String {
        let (command, stream) = if self.jobs.contains(&call) {
            (
                self.tools.get(&call).cloned(),
                self.streams.get(&call).copied(),
            )
        } else {
            (self.tools.remove(&call), self.streams.remove(&call))
        };
        let command = command.unwrap_or_default();
        let stream = stream.map(|stream| self.places.stream(stream));
        let stream = log.or(stream);
        self.detached.remove(&call);
        let id = call.to_string();
        Tag::from(tag::BackgroundResult {
            call: &id,
            command: &command,
            exit: None,
            log: stream.as_deref(),
            output: output.or(Some(note)),
        })
        .to_string()
    }

    /// Appends `text` to the result of call `call`, if that result is in
    /// the last message still; `Err` gives the text back otherwise, for it
    /// to join as the user's.
    fn join_result(&mut self, call: CallId, text: String) -> Result<(), String> {
        let at = self.answered.iter().position(|&c| c == call);
        let (Some(Message::Tool(answers)), Some(at)) = (self.messages.last_mut(), at) else {
            return Err(text);
        };
        let result = &mut answers.results[at].output;
        let mut parts = match std::mem::take(result) {
            llm::Output::Content(parts) => parts,
            output => vec![llm::UserPart::Text(llm::Text::new(text_of(output)))],
        };
        parts.push(llm::UserPart::Text(llm::Text::new(text)));
        *result = llm::Output::Content(parts);
        Ok(())
    }

    /// The seq of the last entry folded.
    pub const fn through(&self) -> u64 {
        self.through
    }

    /// Whether the tools of `registry` presented the results.
    pub fn is_on(&self, registry: &Arc<Registry>) -> bool {
        Arc::ptr_eq(&self.registry, registry)
    }

    pub(super) fn retain(&mut self) {
        self.resident.retain(&mut self.messages);
    }

    pub(super) fn accepts(&self, entry: &Entry) -> bool {
        if !self.resident.has_prefix() {
            return true;
        }
        match entry.body {
            Body::Context { .. } | Body::Spawned { .. } | Body::Edit { .. } => false,
            Body::ToolResult { .. } | Body::Take { into: Some(_), .. } => !self.messages.is_empty(),
            _ => true,
        }
    }

    pub(super) fn request_before(
        &self,
        before: u64,
        restore: impl Fn() -> Result<Messages, llm::Error> + Send + Sync + 'static,
    ) -> Option<llm::Conversation> {
        (self.last < before).then(|| {
            llm::Conversation::retained(self.resident.clone(), self.messages.clone(), restore)
        })
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

fn context_edit(entry: &Entry) -> Option<(u64, u64, Option<String>)> {
    match &entry.body {
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
    }
}

#[cfg(test)]
fn context_edits(
    entries: impl IntoIterator<Item = impl Borrow<Entry>>,
) -> Vec<(u64, u64, Option<String>)> {
    entries
        .into_iter()
        .filter_map(|e| context_edit(e.borrow()))
        .collect()
}

/// Disjoint intervals select the latest edit, including edits of edit entries.
struct Edits {
    edits: Vec<(u64, u64, Option<String>)>,
    spans: BTreeMap<u64, Option<usize>>,
}

impl Edits {
    fn new(edits: Vec<(u64, u64, Option<String>)>) -> Self {
        let mut events = BTreeMap::<u64, Vec<(usize, bool)>>::new();
        for (index, (from, to, _)) in edits.iter().enumerate() {
            if from > to {
                continue;
            }
            events.entry(*from).or_default().push((index, true));
            if let Some(end) = to.checked_add(1) {
                events.entry(end).or_default().push((index, false));
            }
        }
        let mut active = BTreeSet::new();
        let mut spans = BTreeMap::new();
        for (at, events) in events {
            for (index, starts) in events {
                if starts {
                    active.insert(index);
                } else {
                    active.remove(&index);
                }
            }
            spans.insert(at, active.last().copied());
        }
        Self { edits, spans }
    }

    fn at(&self, seq: Seq) -> Option<(u64, &Option<String>)> {
        let index = self.spans.range(..=seq.get()).next_back()?.1.as_ref()?;
        let (from, _, text) = &self.edits[*index];
        Some((*from, text))
    }
}

/// Edits stay in their owning layer; only inherited anchors are rebased.
pub(crate) fn context_image(entries: &[Entry]) -> ContextImage {
    let edits = Edits::new(entries.iter().filter_map(context_edit).collect());
    let completed = entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolResult { call, .. } => Some(call.0),
            Body::Take { input, .. } => Some(*input),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut image = vec![ContextLayer::default()];
    for e in entries {
        let value = if let Some((from, text)) = edits.at(e.seq) {
            if from != e.seq.get() {
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
            if matches!(
                value,
                ContextValue::ToolCall(..)
                    | ContextValue::Queued(..)
                    | ContextValue::Inbox(..)
                    | ContextValue::Child(_)
                    | ContextValue::Background(..)
                    | ContextValue::Job(..)
            ) && !completed.contains(&e.seq)
            {
                continue;
            }
            value
        };
        image[0].1.push(ContextInput(e.seq, value));
    }
    image
}

fn project(
    image: ContextImage,
    registry: Arc<Registry>,
    system: &str,
    places: &Places,
) -> Transcript {
    let mut inherited = HashMap::new();
    for (scope, ContextLayer(runner, inputs)) in image.into_iter().enumerate().rev() {
        let mut transcript = Transcript::empty(
            Arc::clone(&registry),
            if scope == 0 { system } else { "" },
            places,
        );
        transcript.model_only = runner == RunnerKind::ModelOnly;
        let jobs: HashSet<_> = inputs
            .iter()
            .filter_map(|ContextInput(_, value)| match value {
                ContextValue::Job(call, ..) => Some(*call),
                _ => None,
            })
            .collect();
        let completions: HashSet<_> = inputs
            .iter()
            .filter_map(|ContextInput(_, value)| match value {
                ContextValue::Job(call, ..) | ContextValue::Background(call, ..) => Some(*call),
                _ => None,
            })
            .collect();
        for ContextInput(seq, value) in inputs {
            if let ContextValue::ToolCall(_, tool) = &value
                && completions.contains(&CallId(seq))
            {
                transcript.tools.insert(CallId(seq), tool.clone());
                if jobs.contains(&CallId(seq)) {
                    transcript.jobs.insert(CallId(seq));
                }
            }
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
    Transcript::empty(registry, system, places)
}

#[cfg(test)]
#[path = "transcript/context_image_tests.rs"]
mod context_image_tests;

#[cfg(test)]
#[path = "transcript/queue_tests.rs"]
mod queue_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use ext::point::{PresentResult, Presenting};
    use ext::{Extension, Presented, Registrar, Rewrite};
    use protocol::{BlobId, BlobOrigin, BlobRef, EnvelopeId, Epoch, Json, SessionId};

    fn entry(seq: u64, body: Body) -> Entry {
        Entry::new(Seq::new(seq).unwrap(), Epoch(0), body)
    }

    /// A result shows the model the images every provider reads, after what
    /// its presentation showed already, media included; any other image it
    /// holds is only named.
    #[test]
    fn a_result_shows_images_every_provider_reads_after_what_it_showed() {
        let placed = |media_type: &str| ext::Placed {
            held: Held::Blob(BlobRef {
                id: BlobId([1; 32]),
                size: 3,
                media_type: media_type.into(),
                origin: BlobOrigin::ToolOutput(CallId::new(1).unwrap()),
            }),
            at: Some(format!("/blobs/{media_type}")),
        };
        let media = |media_type: &str, source| {
            llm::UserPart::Media(llm::Media {
                media_type: media_type.into(),
                source,
                meta: llm::Meta::new(),
            })
        };
        let drawn = media(
            "image/png",
            llm::Source::Data {
                data: "AA==".into(),
            },
        );
        let text = |text: &str| llm::UserPart::Text(llm::Text::new(text));
        let output = with_held(
            llm::Output::Content(vec![text("look"), drawn.clone()]),
            &[placed("image/png"), placed("image/gif")],
            |_| Some(Arc::from(&b"img"[..])),
        );
        let read = media(
            "image/png",
            llm::Source::Bytes {
                data: Arc::from(&b"img"[..]),
            },
        );
        assert_eq!(
            output,
            llm::Output::Content(vec![
                text("look"),
                drawn,
                text("[image: /blobs/image/png]\n[full output: /blobs/image/gif]"),
                read,
            ])
        );
    }

    fn input(segments: impl Into<Segments>, mode: Mode, taken: bool) -> Body {
        Body::Input {
            segments: segments.into(),
            mode,
            taken,
            origin: None,
            force: false,
        }
    }

    fn take(input: u64) -> Body {
        Body::Take {
            input: Seq::new(input).unwrap(),
            into: None,
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
            id: BlobId([0xab; 32]),
            size: 9000,
            media_type: "text/plain".into(),
            origin: BlobOrigin::Paste,
        };
        let segments = vec![
            Segment::Text("see ".into()),
            Segment::Paste("a\nb".into()),
            Segment::PasteRef(blob),
        ];
        let entries = [entry(1, input(segments, Mode::Queue, true))];
        assert_eq!(
            said(&seen(&entries, Registry::default())),
            [format!(
                "see <pasted-content lines=\"2\" bytes=\"3\">\na\nb\n</pasted-content>\
              <pasted-content path=\"by-id/{}\" bytes=\"9000\"/>",
                "ab".repeat(32)
            )]
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
        let envelope = protocol::Envelope::new(
            EnvelopeId {
                sender,
                call: CallId::new(3).unwrap(),
                purpose: "subagent:message/0".into(),
            },
            SessionId::new(),
            "hello".into(),
            Vec::new(),
        )
        .unwrap();
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
            Segment::Text("see </steer> ".into()),
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
            let transcript = project(
                inputs(runner),
                Arc::new(Registry::default()),
                "",
                &Places::default(),
            );
            said(&transcript.into_messages())
        };
        assert_eq!(read(RunnerKind::ModelOnly), ["see </steer> x"]);
        assert_eq!(
            read(RunnerKind::Agent),
            [
                "<steer from=\"user\">\nsee &lt;/steer> <pasted-content lines=\"1\" bytes=\"1\">\nx\n</pasted-content>\n</steer>"
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
                        output: Json::encode("ok").unwrap().into(),
                    },
                    card: None,
                    held: Vec::new(),
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

    #[test]
    fn an_encoded_tool_tail_is_reconstructed_before_a_late_join() {
        let registry = Arc::new(Registry::default());
        let mut entries = vec![
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
                        output: Json::encode("ok").unwrap().into(),
                    },
                    card: None,
                    held: Vec::new(),
                },
            ),
            entry(3, input("later", Mode::Queue, false)),
        ];
        let mut transcript = Transcript::of(&entries, Arc::clone(&registry), "");
        let saved = entries.clone();
        let source_registry = Arc::clone(&registry);
        let request = llm::Request {
            model: "m".into(),
            messages: transcript
                .request_before(4, move || {
                    Ok(Transcript::of(&saved, Arc::clone(&source_registry), "").into_messages())
                })
                .unwrap(),
            ..llm::Request::default()
        };
        let provider = llm::Provider::new(
            "openai",
            "http://127.0.0.1:1",
            llm::Auth::None,
            llm::protocols::chat::Chat::new(llm::protocols::chat::HOSTS[0].1),
        );
        let prepared = llm::prepare_request(&provider, &request);
        drop(request);
        transcript.retain();
        assert!(transcript.messages.is_empty());
        let joined = entry(
            4,
            Body::Take {
                input: Seq::new(3).unwrap(),
                into: Some(CallId::new(1).unwrap()),
            },
        );
        assert!(!transcript.apply(&joined, None));
        entries.push(joined);
        let replayed = Transcript::of(&entries, registry, "").into_messages();
        let Some(Message::Tool(tool)) = replayed.last() else {
            panic!("joined tool result")
        };
        let llm::Output::Content(parts) = &tool.results[0].output else {
            panic!("joined content")
        };
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, llm::UserPart::Text(text) if text.text == "later"))
        );
        std::hint::black_box(prepared.origin());
    }
}
