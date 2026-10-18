//! Chat Completions frames into parts: one lenient state machine for a
//! stream of chunks and for a whole `chat.completion` in one frame, which is
//! the one chunk carrying its message. All of a message's reasoning is one
//! part, open until the message ends and held first however late it
//! arrived; a run of text is a part, as is a run of Mistral thinking parts;
//! each tool call, image and unknown content part is a part of its own. An
//! answer's audio transcript is its text when it has no other, and its text
//! part holds the audio's id.

use serde_json::{Map, Value, json};

use super::merge::{REASONING_DETAILS, merge_details, merge_fields};
use super::{KEY, Quirks};
use crate::error::Error;
use crate::json::Lenient;
use crate::protocol::{Decoder, Flow, Frame};
use crate::spec::{Event, Media, Reason, Source, Usage};
use crate::writer::{AUTO_INDEX, CallFragment, End, Open, Writer};

/// The finish reasons every host shares: `OpenAI`'s, the legacy
/// `function_call`, and the spellings compatible servers use for the same
/// endings.
const CHAT_FINISHES: &[(&str, Reason)] = &[
    ("stop", Reason::Stop),
    ("end", Reason::Stop),
    ("eos", Reason::Stop),
    ("end_turn", Reason::Stop),
    ("stop_sequence", Reason::Stop),
    ("length", Reason::Length),
    ("max_tokens", Reason::Length),
    ("model_length", Reason::Length),
    ("tool_calls", Reason::ToolCalls),
    ("function_call", Reason::ToolCalls),
    ("content_filter", Reason::ContentFilter),
];

/// The upstreams' own reasons a gateway forwards beside its normalized one,
/// compared lowercased.
const NATIVE_FINISHES: &[(&str, Reason)] = &[
    ("stop", Reason::Stop),
    ("end_turn", Reason::Stop),
    ("stop_sequence", Reason::Stop),
    ("complete", Reason::Stop),
    ("completed", Reason::Stop),
    ("length", Reason::Length),
    ("max_tokens", Reason::Length),
    ("max_output_tokens", Reason::Length),
    ("model_length", Reason::Length),
    ("tool_calls", Reason::ToolCalls),
    ("function_call", Reason::ToolCalls),
    ("tool_use", Reason::ToolCalls),
    ("content_filter", Reason::ContentFilter),
    ("safety", Reason::ContentFilter),
    ("blocklist", Reason::ContentFilter),
    ("prohibited_content", Reason::ContentFilter),
    ("spii", Reason::ContentFilter),
];

/// `reason` in the host's documented vocabulary and the shared one; any
/// other is [`Reason::Other`].
pub(super) fn finish_reason(reason: &str, quirks: &Quirks) -> Reason {
    quirks
        .finishes
        .iter()
        .chain(CHAT_FINISHES)
        .find(|(name, _)| *name == reason)
        .map_or(Reason::Other, |(_, finish)| *finish)
}

fn native_finish_reason(reason: &str) -> Reason {
    NATIVE_FINISHES
        .iter()
        .find(|(name, _)| *name == reason)
        .map_or(Reason::Other, |(_, finish)| *finish)
}

/// The keys an assistant message carries reasoning under, in the order
/// their text is read: servers that send several send the same text under
/// each, so the first that carries text is the one kept.
pub(super) const REASONING_TEXT_KEYS: [&str; 3] =
    ["reasoning_content", "reasoning", "reasoning_text"];

/// What one content part of a message is.
enum Part {
    /// A `text` part, or a `refusal` part's text.
    Text(String),
    /// Mistral's reasoning: a `thinking` part's text.
    Thinking(String),
    Image,
    /// A part with no shared meaning, kept as it came.
    Unknown,
}

impl Part {
    /// The part `part` is, by its `type`; one with no `type` and a string
    /// `text` is text.
    fn of(part: &Value) -> Self {
        let text = |key: &str| part.str(key).unwrap_or_default().to_owned();
        match part.str("type") {
            Some("text") => Self::Text(text("text")),
            None if part.str("text").is_some() => Self::Text(text("text")),
            Some("refusal") => Self::Text(text("refusal")),
            Some("thinking") => Self::Thinking(match part.get("thinking") {
                Some(Value::Array(chunks)) => chunks
                    .iter()
                    .filter_map(|chunk| chunk.str("text"))
                    .collect(),
                _ => text("thinking"),
            }),
            Some("image_url") => Self::Image,
            _ => Self::Unknown,
        }
    }
}

/// What one tool call of a message is, by its `type`.
#[derive(PartialEq, Eq)]
enum CallKind {
    /// Arguments as JSON text; a call with no `type` is one.
    Function,
    /// Free-text input, the call's `{"input"}`.
    Custom,
    /// A kind nothing can answer, kept but never sent back.
    Unknown,
}

impl CallKind {
    fn of(call: &Value) -> Self {
        match call.str("type") {
            None | Some("function") => Self::Function,
            Some("custom") => Self::Custom,
            Some(_) => Self::Unknown,
        }
    }
}

/// The kind of run the decoder is writing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Writing {
    Text,
    Thinking,
}

/// A tool call the reply has open.
struct OpenCall {
    at: usize,
    id: Option<String>,
    /// A kind nothing can answer: an opaque part.
    opaque: bool,
    arguments: String,
}

/// Whether `text` is a whole JSON object; nothing at all is one.
fn whole_object(text: &str) -> bool {
    text.trim().is_empty() || matches!(serde_json::from_str(text), Ok(Value::Object(_)))
}

pub(super) struct ChatDecoder {
    w: Writer,
    quirks: Quirks,
    /// The run being written and its index.
    writing: Option<(Writing, usize)>,
    thinking_text: String,
    /// The message's reasoning part while it is open, and its item as it
    /// will hold it: the field its text arrived in, its text, and every
    /// `reasoning_details` entry in order.
    reasoning: Option<usize>,
    reasoning_field: Option<&'static str>,
    reasoning_text: String,
    reasoning_details: Map<String, Value>,
    audio_id: Option<Value>,
    /// The calls still open, in the order they opened.
    calls: Vec<OpenCall>,
    usage: Option<Value>,
    finish: Option<(Reason, String)>,
    response_id: Option<String>,
    response_model: Option<String>,
    /// Whether a finish reason or a whole reply ended the turn.
    ended: bool,
    /// Whether a stream chunk arrived.
    chunked: bool,
}

impl ChatDecoder {
    pub(super) fn new(quirks: Quirks) -> Self {
        Self {
            w: Writer::new(KEY).with_call_id_slot(),
            quirks,
            writing: None,
            thinking_text: String::new(),
            reasoning: None,
            reasoning_field: None,
            reasoning_text: String::new(),
            reasoning_details: Map::new(),
            audio_id: None,
            calls: Vec::new(),
            usage: None,
            finish: None,
            response_id: None,
            response_model: None,
            ended: false,
            chunked: false,
        }
    }

    /// Absorb what every frame carries, and take its primary choice. Usage
    /// is the frame's, or its choice's where Moonshot reports it.
    fn absorb(&mut self, frame: &Value) -> Option<Value> {
        if let Some(id) = frame.str("id") {
            self.response_id = Some(id.to_owned());
        }
        if let Some(model) = frame.str("model") {
            self.response_model = Some(model.to_owned());
        }
        // `n > 1` streams interleave candidates told apart only by their
        // `index`; candidate 0 is the turn.
        let choice = frame
            .arr("choices")
            .iter()
            .find(|choice| {
                choice
                    .get("index")
                    .and_then(Value::as_u64)
                    .is_none_or(|index| index == 0)
            })
            .cloned();
        if let Some(usage) = frame
            .at("/usage")
            .or_else(|| choice.as_ref().and_then(|choice| choice.at("/usage")))
        {
            self.usage = Some(usage.clone());
        }
        let choice = choice?;
        // A gateway's upstream-native reason stands in only when the
        // normalized one is absent or empty.
        let reason = match choice
            .str("finish_reason")
            .filter(|reason| !reason.is_empty())
        {
            Some(reason) => Some((finish_reason(reason, &self.quirks), reason.to_owned())),
            None => choice
                .str("native_finish_reason")
                .filter(|reason| self.quirks.native_finish_reason && !reason.is_empty())
                .map(|reason| {
                    let reason = reason.to_ascii_lowercase();
                    (native_finish_reason(&reason), reason)
                }),
        };
        if let Some(reason) = reason {
            self.finish = Some(reason);
            self.ended = true;
        }
        Some(choice)
    }

    /// One delta of the assistant message: its reasoning text and details
    /// into the reasoning part, then the content, images and calls.
    fn delta(&mut self, delta: &Value) -> Result<(), Error> {
        let reasoning = REASONING_TEXT_KEYS.iter().find_map(|key| {
            delta
                .str(key)
                .filter(|text| !text.is_empty())
                .map(|text| (*key, text))
        });
        if let Some((key, text)) = reasoning {
            self.reason(text)?;
            self.reasoning_field.get_or_insert(key);
        }
        if let Some(Value::Array(details)) = delta.get(REASONING_DETAILS)
            && !details.is_empty()
        {
            self.reason("")?;
            merge_details(&mut self.reasoning_details, details.clone());
        }
        let audio = delta.get("audio");
        if let Some(id) = audio.and_then(|audio| audio.at("/id")) {
            self.audio_id = Some(id.clone());
        }
        match delta.get("content") {
            Some(Value::Array(parts)) => {
                for part in parts {
                    self.part(part)?;
                }
            }
            Some(part @ Value::Object(_)) => self.part(part)?,
            _ => {
                let text = ["content", "refusal"]
                    .iter()
                    .find_map(|key| delta.str(key).filter(|text| !text.is_empty()))
                    .or_else(|| audio.and_then(|audio| audio.str("transcript")))
                    .filter(|text| !text.is_empty());
                if let Some(text) = text {
                    self.write(Writing::Text, text)?;
                }
            }
        }
        for image in delta.arr("images") {
            self.image(image)?;
        }
        for call in delta.arr("tool_calls") {
            self.call(call)?;
        }
        Ok(())
    }

    /// Append `text` to the message's reasoning part, opening it first.
    fn reason(&mut self, text: &str) -> Result<(), Error> {
        let at = if let Some(at) = self.reasoning {
            at
        } else {
            let at = self.w.fresh_index();
            self.w
                .open(at, Open::Reasoning { redacted: false }, Value::Null)?;
            self.w.lead(at)?;
            self.reasoning = Some(at);
            at
        };
        self.reasoning_text.push_str(text);
        self.w.push(at, text)
    }

    /// Close the message's reasoning part, holding its item: the field its
    /// text arrived in, and its details.
    fn close_reasoning(&mut self) -> Result<(), Error> {
        let Some(at) = self.reasoning.take() else {
            return Ok(());
        };
        let text = std::mem::take(&mut self.reasoning_text);
        let mut item = Map::new();
        if let Some(field) = self.reasoning_field.take() {
            item.insert(field.to_owned(), text.into());
        }
        item.append(&mut self.reasoning_details);
        self.w.edit(at, |slot| *slot = Value::Object(item))?;
        self.w.finish(at)
    }

    /// Append `text` to a run of kind `writing`, first closing the run being
    /// written when it is of another kind.
    fn write(&mut self, writing: Writing, text: &str) -> Result<(), Error> {
        let index = match self.writing {
            Some((current, index)) if current == writing => index,
            _ => {
                self.close_writing()?;
                let index = self.w.fresh_index();
                let block = match writing {
                    Writing::Thinking => Open::Reasoning { redacted: false },
                    Writing::Text => Open::Text,
                };
                self.w.open(index, block, Value::Null)?;
                self.writing = Some((writing, index));
                index
            }
        };
        if writing == Writing::Thinking {
            self.thinking_text.push_str(text);
        }
        self.w.push(index, text)
    }

    /// Close the run being written, holding its item: the thinking part, or
    /// the audio's id.
    fn close_writing(&mut self) -> Result<(), Error> {
        let (index, item) = match self.writing.take() {
            None => return Ok(()),
            Some((Writing::Text, index)) => (
                index,
                self.audio_id
                    .take()
                    .map(|id| json!({ "audio": { "id": id } })),
            ),
            Some((Writing::Thinking, index)) => {
                let text = std::mem::take(&mut self.thinking_text);
                (
                    index,
                    Some(json!({"type": "thinking", "thinking": [{"type": "text", "text": text}]})),
                )
            }
        };
        if let Some(item) = item {
            self.w.edit(index, |slot| *slot = item)?;
        }
        self.w.finish(index)
    }

    fn part(&mut self, part: &Value) -> Result<(), Error> {
        match Part::of(part) {
            Part::Text(text) => self.write(Writing::Text, &text),
            Part::Thinking(text) => self.write(Writing::Thinking, &text),
            Part::Image => self.image(part),
            Part::Unknown => {
                self.close_writing()?;
                let index = self.w.fresh_index();
                self.w
                    .whole(index, Open::Opaque { replay: true }, part.clone(), "")
            }
        }
    }

    /// One image the model produced, whole: a data URL is base64 data, any
    /// other URL stays a URL, of a type it does not name.
    fn image(&mut self, part: &Value) -> Result<(), Error> {
        let url = part
            .at("/image_url/url")
            .or_else(|| part.get("image_url"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let media = match url
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(";base64,"))
        {
            Some((media_type, data)) => Media {
                media_type: media_type.to_owned(),
                source: Source::Data {
                    data: data.to_owned(),
                },
                meta: crate::Meta::new(),
            },
            None => Media {
                media_type: "image/*".to_owned(),
                source: Source::Url {
                    url: url.to_owned(),
                },
                meta: crate::Meta::new(),
            },
        };
        self.close_writing()?;
        let index = self.w.fresh_index();
        self.w.open(index, Open::Media(media), Value::Null)?;
        self.w.finish(index)
    }

    /// One tool-call fragment. It goes to the call at its wire index, where
    /// the writer tells a new id under it from the call it held. Without an
    /// index it goes to the open call that states its id; one that states
    /// no id continues the latest call while that call's arguments are not
    /// yet a whole object, and otherwise opens a new call when it brings a
    /// name or arguments. Its fields merge into the call's item.
    fn call(&mut self, call: &Value) -> Result<(), Error> {
        self.close_writing()?;
        let id = match call.get("id") {
            Some(Value::String(id)) if !id.is_empty() && id != "null" => Some(id.clone()),
            Some(id @ Value::Number(_)) => Some(id.to_string()),
            _ => None,
        };
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < AUTO_INDEX);
        let arguments = call
            .at("/function/arguments")
            .map(|arguments| match arguments {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            });
        let name = call
            .at("/function/name")
            .or_else(|| call.at("/custom/name"))
            .and_then(Value::as_str);
        let starts = name.is_some_and(|name| !name.is_empty())
            || arguments.as_deref().is_some_and(|text| !text.is_empty());
        let at = match index {
            Some(index) => index,
            None => match id.as_deref() {
                Some(id) => self
                    .calls
                    .iter()
                    .find(|open| open.id.as_deref() == Some(id)),
                None => self
                    .calls
                    .last()
                    .filter(|open| open.opaque || !whole_object(&open.arguments) || !starts),
            }
            .map_or_else(|| self.w.fresh_index(), |open| open.at),
        };
        let opaque = if let Some(open) = self.calls.iter_mut().find(|open| open.at == at) {
            if id.is_some() {
                open.id.clone_from(&id);
            }
            open.arguments
                .push_str(arguments.as_deref().unwrap_or_default());
            open.opaque
        } else {
            let opaque = CallKind::of(call) == CallKind::Unknown;
            if opaque {
                self.w
                    .open(at, Open::Opaque { replay: false }, Value::Null)?;
            }
            self.calls.push(OpenCall {
                at,
                id: id.clone(),
                opaque,
                arguments: arguments.clone().unwrap_or_default(),
            });
            opaque
        };
        if !opaque {
            self.w.fragment(
                Some(at),
                CallFragment {
                    id: id.as_deref(),
                    name,
                    arguments: arguments.as_deref(),
                },
            )?;
        }
        let mut fields = call.as_object().cloned().unwrap_or_default();
        // The index orders the stream; the call it assembles has none.
        fields.remove("index");
        self.w.edit(at, |item| {
            if !item.is_object() {
                *item = Value::Object(Map::new());
            }
            if let Value::Object(item) = item {
                merge_fields(item, &fields);
            }
        })
    }

    /// Close the open calls with their items: every one at the end (`all`),
    /// only those whose arguments are whole at a finish chunk, since a
    /// provider may still send fragments after it. A custom call's arguments
    /// are its `{"input"}`. A call the output budget cut before its arguments
    /// were whole keeps no item: the provider never stated it complete.
    fn close_calls(&mut self, all: bool) -> Result<(), Error> {
        let cut = matches!(self.finish, Some((Reason::Length, _)));
        let mut waiting = Vec::new();
        for call in std::mem::take(&mut self.calls) {
            if call.opaque {
                self.w.finish(call.at)?;
                continue;
            }
            let (mut input, mut complete) = (None, false);
            self.w.edit(call.at, |item| {
                if CallKind::of(item) == CallKind::Custom {
                    let custom = item.at("/custom/input").cloned();
                    input = Some(custom.unwrap_or_else(|| Value::from("")));
                }
                complete = match item.at("/function/arguments") {
                    Some(Value::String(text)) => whole_object(text),
                    Some(arguments) => arguments.is_object(),
                    None => input.is_some(),
                };
            })?;
            if !all && !complete {
                waiting.push(call);
                continue;
            }
            if let Some(input) = input {
                self.w.announce(call.at, json!({ "input": input }))?;
            }
            if cut && !complete {
                self.w.close(call.at)?;
            } else {
                self.w.finish(call.at)?;
            }
        }
        self.calls = waiting;
        Ok(())
    }

    /// One `chat.completion.chunk`.
    fn chunk(&mut self, frame: &Value) -> Result<(), Error> {
        let Some(choice) = self.absorb(frame) else {
            return Ok(());
        };
        if let Some(delta) = choice.get("delta").filter(|delta| delta.is_object()) {
            self.delta(delta)?;
        }
        // Text and reasoning stay open until the stream ends: a provider may
        // still send either after the finish chunk.
        if matches!(self.finish, Some((Reason::ToolCalls, _))) {
            self.close_calls(false)?;
        }
        Ok(())
    }

    /// The `chat.completion` body: the one chunk whose delta is its whole
    /// message, each call at its position in the list, then the end.
    fn whole(&mut self, frame: &Value, data: &str) -> Result<Flow, Error> {
        let Some(choice) = self.absorb(frame) else {
            return Err(Error::decode("the reply holds no choices", data));
        };
        let Some(mut message) = choice.get("message").filter(|m| m.is_object()).cloned() else {
            return Err(Error::decode("the reply holds no message", data));
        };
        self.ended = true;
        if let Some(Value::Array(calls)) = message.get_mut("tool_calls") {
            for (index, call) in calls.iter_mut().enumerate() {
                if let Some(call) = call.as_object_mut() {
                    call.insert("index".to_owned(), index.into());
                }
            }
        }
        self.delta(&message)?;
        self.close_calls(true)?;
        self.end()
    }

    /// Write the provider's end of the reply.
    fn end(&mut self) -> Result<Flow, Error> {
        self.close_writing()?;
        self.close_reasoning()?;
        let usage = self
            .usage
            .as_ref()
            .map_or_default(|usage| usage_of(usage, &self.quirks));
        self.w.end(End {
            usage,
            reason: self.finish.take().map(|(reason, raw)| (reason, Some(raw))),
            response_id: self.response_id.take(),
            model: self.response_model.take(),
            meta: super::super::usage_meta(KEY, self.usage.as_ref()),
            ..End::default()
        })
    }

    /// The stream ended: flush the calls the provider delivered, then end
    /// the reply. A stream no finish reason ended was cut short.
    fn finish(&mut self) -> Result<Flow, Error> {
        if !self.ended {
            return Err(Error::incomplete());
        }
        self.close_calls(true)?;
        self.end()
    }

    /// The `[DONE]` sentinel. A host whose streams omit the finish reason
    /// ends a turn that streamed a chunk: as tool calls when it holds one,
    /// else as a stop.
    fn done(&mut self) -> Result<Flow, Error> {
        if !self.ended && self.chunked && self.quirks.done_without_finish_reason {
            self.finish = Some(if self.calls.is_empty() {
                (Reason::Stop, "stop".to_owned())
            } else {
                (Reason::ToolCalls, "tool_calls".to_owned())
            });
            self.ended = true;
        }
        self.finish()
    }

    fn decode(&mut self, data: &str) -> Result<Flow, Error> {
        if data.trim() == "[DONE]" {
            return self.done();
        }
        let frame: Value =
            serde_json::from_str(data).map_err(|error| Error::decode(error, data))?;
        if let Some(error) = failure(&frame) {
            return Err(error);
        }
        if let (true, Value::String(text)) = (self.quirks.bare_string_reply, &frame) {
            // A bare string is the whole answer, ended as a stop.
            self.ended = true;
            self.finish = Some((Reason::Stop, "stop".to_owned()));
            let text = text.clone();
            self.write(Writing::Text, &text)?;
            return self.end();
        }
        // Without a `type`, a frame is a chunk when it says so or carries
        // choices; anything else is not this wire's and is skipped.
        let object = frame.str("object");
        if !frame.is_object()
            || !(object == Some("chat.completion.chunk") || frame.get("choices").is_some())
        {
            return Ok(Flow::More);
        }
        // The `object` tag decides when the host sends one: a stream chunk
        // may carry a whole message.
        let whole = match object {
            Some(object) => object == "chat.completion",
            None => frame
                .arr("choices")
                .iter()
                .any(|choice| choice.obj("message").is_some()),
        };
        if whole {
            return self.whole(&frame, data);
        }
        self.chunked = true;
        self.chunk(&frame)?;
        Ok(Flow::More)
    }
}

impl Decoder for ChatDecoder {
    fn frame(&mut self, frame: Frame<'_>, out: &mut Vec<Event>) -> Result<Flow, Error> {
        let flow = self.decode(frame.data);
        self.w.drain(out);
        flow
    }

    /// A stream that stops after a finish reason without `[DONE]` still
    /// ended: some hosts (Perplexity) never send the sentinel.
    fn end(&mut self, out: &mut Vec<Event>) -> Result<(), Error> {
        let ended = self.finish().map(drop);
        self.w.drain(out);
        ended
    }
}

/// The in-band error envelope, delivered with a 200 status. A `null` or
/// empty `error` can accompany valid terminal usage, and only a populated
/// `choices` array outranks the error.
fn failure(frame: &Value) -> Option<Error> {
    let error = frame
        .get("error")
        .filter(|error| error.is_object() || error.as_str().is_some_and(|text| !text.is_empty()))?;
    if !frame.arr("choices").is_empty() {
        return None;
    }
    let code = error
        .get("code")
        .and_then(|code| match code {
            Value::String(code) => Some(code.clone()),
            Value::Number(code) => Some(code.to_string()),
            _ => None,
        })
        .or_else(|| error.str("type").map(str::to_owned));
    let message = error
        .as_str()
        .or_else(|| error.str("message"))
        .map_or_else(|| frame.to_string(), str::to_owned);
    Some(Error::provider(code.as_deref(), message))
}

/// A usage object read leniently, so no shape fails a reply: a counter that
/// is absent or not a non-negative integer is unreported, and one inside a
/// details object the usage has is zero. Cached input falls back to
/// Mistral's `num_cached_tokens`, then `DeepSeek`'s
/// `prompt_cache_hit_tokens`; audio input counts as input when the total
/// says it sits beside the prompt; output is the rest of the total when
/// unreported; and the reasoning count is left out where the host's cannot
/// be trusted.
fn usage_of(usage: &Value, quirks: &Quirks) -> Usage {
    let count = |pointer: &str| usage.at(pointer).and_then(Value::as_u64);
    let detail = |object: &str, key: &str| {
        count(&format!("/{object}/{key}")).or(usage.obj(object).map(|_| 0))
    };
    let (prompt, completion, total) = (
        count("/prompt_tokens"),
        count("/completion_tokens"),
        count("/total_tokens"),
    );
    let audio = count("/prompt_tokens_details/audio_tokens").unwrap_or(0);
    let input = prompt.map(|prompt| {
        let beside = prompt.saturating_add(audio);
        let accounted = beside.saturating_add(completion.unwrap_or(0));
        if audio != 0 && Some(accounted) == total {
            beside
        } else {
            prompt
        }
    });
    Usage {
        input,
        output: completion.or_else(|| total.map(|total| total.saturating_sub(input.unwrap_or(0)))),
        cache_read: detail("prompt_tokens_details", "cached_tokens")
            .or_else(|| count("/num_cached_tokens"))
            .or_else(|| count("/prompt_cache_hit_tokens")),
        cache_write: count("/prompt_tokens_details/cache_write_tokens")
            .or_else(|| count("/cache_creation_input_tokens")),
        audio_input: count("/prompt_tokens_details/audio_tokens").filter(|tokens| *tokens > 0),
        reasoning: detail("completion_tokens_details", "reasoning_tokens")
            .filter(|_| quirks.reliable_reasoning_count),
        audio_output: count("/completion_tokens_details/audio_tokens").filter(|tokens| *tokens > 0),
        ..Usage::default()
    }
}
