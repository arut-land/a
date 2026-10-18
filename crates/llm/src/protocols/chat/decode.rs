//! Chat Completions frames into parts: one lenient state machine for a
//! stream of chunks and for a whole `chat.completion` in one frame, which is
//! the one chunk carrying its message. All of a message's reasoning is one
//! part, open until the message ends and held first however late it
//! arrived; a run of text is a part, as is a run of Mistral thinking parts;
//! each tool call, image and unknown content part is a part of its own. An
//! answer's audio transcript is its text when it has no other, and its text
//! part holds the audio's id.
//!
//! A frame is read into structs that borrow its strings ([`Json`]), so a
//! token costs no allocation beyond the text its event owns, and a field of
//! an unexpected type reads as what it is rather than failing the frame.

use std::borrow::Cow;

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Map, Value, json};

use super::merge::{merge_details, merge_fields, merge_object, merge_str};
use super::{KEY, PartKind, Quirks, ReasoningField};
use crate::error::Error;
use crate::json::{Json, Lenient, whole_object};
use crate::protocol::Flow;
use crate::spec::{Media, Meta, Reason, SmolStr, Source, Usage};
use crate::writer::{AUTO_INDEX, CallFragment, End, Open, Writer, Writes};

/// `reason` in the host's documented vocabulary, then the shared one.
pub(super) fn finish_reason(reason: &str, quirks: &Quirks) -> Reason {
    quirks
        .finishes
        .iter()
        .find(|(name, _)| *name == reason)
        .map_or_else(|| shared_finish(reason), |(_, finish)| *finish)
}

/// `OpenAI`'s reasons, the legacy `function_call`, and the spellings
/// compatible servers use for the same endings.
fn shared_finish(reason: &str) -> Reason {
    match reason {
        "stop" | "end" | "eos" | "end_turn" | "stop_sequence" => Reason::Stop,
        "length" | "max_tokens" | "model_length" => Reason::Length,
        "tool_calls" | "function_call" => Reason::ToolCalls,
        "content_filter" => Reason::ContentFilter,
        _ => Reason::Other,
    }
}

/// The upstreams' own reasons a gateway forwards beside its normalized one,
/// compared lowercased.
fn native_finish(reason: &str) -> Reason {
    match reason {
        "end" | "eos" => Reason::Other,
        "complete" | "completed" => Reason::Stop,
        "max_output_tokens" => Reason::Length,
        "tool_use" => Reason::ToolCalls,
        "safety" | "blocklist" | "prohibited_content" | "spii" => Reason::ContentFilter,
        reason => shared_finish(reason),
    }
}

impl<T> Json<'_, T> {
    /// Merge the value under `key` as [`merge_fields`] merges one, an
    /// object by `object`.
    fn merge<M>(&self, target: &mut Map<String, Value>, key: &str, object: M)
    where
        M: FnOnce(&mut Map<String, Value>, &T),
    {
        match self {
            Self::Str(text) => merge_str(target, key, text),
            Self::Object(value) => object(target, value),
            json => {
                if let Some(value) = json.scalar() {
                    merge_fields(target, [(key, &value)]);
                }
            }
        }
    }
}

impl Json<'_> {
    fn merge_value(&self, target: &mut Map<String, Value>, key: &str) {
        self.merge(target, key, |target, value| {
            merge_fields(target, [(key, value)]);
        });
    }
}

/// An array of `T` objects, or any other value.
type List<'a, T> = Json<'a, IgnoredAny, Json<'a, T>>;

#[derive(Default, Deserialize)]
#[serde(default, bound = "'de: 'a")]
struct Frame<'a> {
    id: Json<'a>,
    model: Json<'a>,
    object: Json<'a>,
    choices: List<'a, Choice<'a>>,
    usage: Value,
    error: Value,
}

#[derive(Default, Deserialize)]
#[serde(default, bound = "'de: 'a")]
struct Choice<'a> {
    index: Value,
    finish_reason: Json<'a>,
    native_finish_reason: Json<'a>,
    usage: Value,
    delta: Json<'a, Delta<'a>>,
    message: Json<'a, Delta<'a>>,
}

/// A delta of the assistant message, or a whole message.
#[derive(Default, Deserialize)]
#[serde(default, bound = "'de: 'a")]
struct Delta<'a> {
    reasoning_content: Json<'a>,
    reasoning: Json<'a>,
    reasoning_text: Json<'a>,
    reasoning_details: Value,
    audio: Value,
    content: Json<'a>,
    refusal: Json<'a>,
    images: Value,
    tool_calls: List<'a, Call<'a>>,
}

/// A tool-call fragment. Every field but `index` joins the call's item.
#[derive(Default, Deserialize)]
#[serde(default, bound = "'de: 'a")]
struct Call<'a> {
    index: Value,
    id: Json<'a>,
    #[serde(rename = "type")]
    kind: Json<'a>,
    function: Json<'a, Function<'a>>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

#[derive(Default, Deserialize)]
#[serde(default, bound = "'de: 'a")]
struct Function<'a> {
    name: Json<'a>,
    arguments: Json<'a>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

impl Call<'_> {
    fn merge(&self, item: &mut Map<String, Value>) {
        self.id.merge_value(item, "id");
        self.kind.merge_value(item, "type");
        self.function.merge(item, "function", |item, function| {
            merge_object(item, "function", |slot| {
                function.name.merge_value(slot, "name");
                function.arguments.merge_value(slot, "arguments");
                merge_fields(slot, &function.rest);
            });
        });
        merge_fields(item, &self.rest);
    }
}

/// What one tool call of a message is, by its `type`: a function (or no
/// `type`), a custom call whose arguments are its `{"input"}`, or a kind
/// nothing can answer, kept but never sent back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CallKind {
    Function,
    Custom,
    Unknown,
}

impl CallKind {
    fn of(kind: Option<&str>) -> Self {
        match kind {
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

/// A tool call the reply has open; `opaque` for a kind nothing can answer.
struct OpenCall {
    at: usize,
    id: Option<String>,
    opaque: bool,
}

pub(super) struct ChatDecoder {
    w: Writer,
    quirks: Quirks,
    /// The run being written and its index.
    writing: Option<(Writing, usize)>,
    /// The message's reasoning part while it is open, and its item as it
    /// will hold it: the field its text arrived in, its text, and every
    /// `reasoning_details` entry in order.
    reasoning: Option<usize>,
    reasoning_field: Option<ReasoningField>,
    reasoning_details: Map<String, Value>,
    audio_id: Option<Value>,
    /// The calls still open, in the order they opened.
    calls: Vec<OpenCall>,
    finish: Option<(Reason, SmolStr)>,
    /// The reply's id, model and usage object, as the end reports them.
    end: End,
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
            reasoning: None,
            reasoning_field: None,
            reasoning_details: Map::new(),
            audio_id: None,
            calls: Vec::new(),
            finish: None,
            end: End::default(),
            ended: false,
            chunked: false,
        }
    }

    /// Absorb what every frame carries, and take its primary choice: `n >
    /// 1` streams interleave candidates told apart only by their `index`,
    /// and candidate 0 is the turn. Usage is the frame's, or its choice's
    /// where Moonshot reports it. A gateway's upstream-native reason stands
    /// in only when the normalized one is absent or empty.
    fn absorb<'a>(&mut self, frame: Frame<'a>) -> Option<Json<'a, Choice<'a>>> {
        for (slot, value) in [
            (&mut self.end.response_id, frame.id.str()),
            (&mut self.end.model, frame.model.str()),
        ] {
            // What every chunk restates is kept, not copied again.
            if let Some(value) = value.filter(|value| slot.as_deref() != Some(*value)) {
                *slot = Some(value.into());
            }
        }
        let primary = |choice: &Json<'_, Choice<'_>>| {
            let index = choice.object().map(|choice| choice.index.as_u64());
            index.is_none_or(|index| index.is_none_or(|index| index == 0))
        };
        let mut choice = match frame.choices {
            Json::List(choices) => choices.into_iter().find(primary),
            _ => None,
        };
        let usage = match (frame.usage, &mut choice) {
            (Value::Null, Some(Json::Object(choice))) => choice.usage.take(),
            (usage, _) => usage,
        };
        if !usage.is_null() {
            self.end.raw_usage = Some(usage);
        }
        let Some(Json::Object(found)) = &choice else {
            return choice;
        };
        let native = found.native_finish_reason.str();
        let reason = match found
            .finish_reason
            .str()
            .filter(|reason| !reason.is_empty())
        {
            Some(reason) => (finish_reason(reason, &self.quirks), reason.into()),
            None => match native
                .filter(|reason| self.quirks.native_finish_reason && !reason.is_empty())
            {
                Some(reason) => {
                    let reason = reason.to_ascii_lowercase();
                    (native_finish(&reason), reason.into())
                }
                None => return choice,
            },
        };
        self.finish = Some(reason);
        self.ended = true;
        choice
    }

    /// One delta of the assistant message: its reasoning text and details
    /// into the reasoning part, then the content, images and calls, each
    /// call of a whole message (`whole`) at its position in the list.
    fn delta(&mut self, delta: Delta<'_>, whole: bool) -> Result<(), Error> {
        let texts = [
            delta.reasoning_content,
            delta.reasoning,
            delta.reasoning_text,
        ];
        if let Some((field, text)) = ReasoningField::first(texts.map(Json::into_text)) {
            self.reason(text)?;
            self.reasoning_field.get_or_insert(field);
        }
        if let Value::Array(details) = delta.reasoning_details
            && !details.is_empty()
        {
            self.reason("")?;
            merge_details(&mut self.reasoning_details, details);
        }
        if let Some(id) = delta.audio.at("/id") {
            self.audio_id = Some(id.clone());
        }
        match delta.content {
            Json::List(parts) => {
                for part in &parts {
                    self.part(part)?;
                }
            }
            Json::Object(part) => self.part(&part)?,
            content => {
                let transcript = delta
                    .audio
                    .str("transcript")
                    .filter(|text| !text.is_empty());
                let text = content.into_text().or_else(|| delta.refusal.into_text());
                if let Some(text) = text.or(transcript.map(Cow::Borrowed)) {
                    self.write(Writing::Text, text)?;
                }
            }
        }
        for image in delta.images.as_array().into_iter().flatten() {
            self.image(image)?;
        }
        for (position, call) in delta.tool_calls.items().iter().enumerate() {
            let call = call.object();
            self.call(call, whole.then_some(position).filter(|_| call.is_some()))?;
        }
        Ok(())
    }

    /// Append `text` to the message's reasoning part, opening it first.
    fn reason<'a>(&mut self, text: impl Into<Cow<'a, str>>) -> Result<(), Error> {
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
        self.w.push(at, text)
    }

    /// Close the message's reasoning part, holding its item: the field its
    /// text arrived in, and its details.
    fn close_reasoning(&mut self) -> Result<(), Error> {
        let Some(at) = self.reasoning.take() else {
            return Ok(());
        };
        let mut item = Map::new();
        if let Some(field) = self.reasoning_field.take() {
            let text = self.w.text(at).unwrap_or_default();
            item.insert(field.as_str().to_owned(), text.into());
        }
        item.append(&mut self.reasoning_details);
        self.w.edit(at, |slot| *slot = Value::Object(item))?;
        self.w.finish(at)
    }

    /// Append `text` to a run of kind `writing`, first closing the run being
    /// written when it is of another kind.
    fn write<'a>(&mut self, writing: Writing, text: impl Into<Cow<'a, str>>) -> Result<(), Error> {
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
        self.w.push(index, text)
    }

    /// Close the run being written, holding its item: the thinking part, or
    /// the audio's id.
    fn close_writing(&mut self) -> Result<(), Error> {
        let Some((writing, index)) = self.writing.take() else {
            return Ok(());
        };
        let item = match writing {
            Writing::Text => self
                .audio_id
                .take()
                .map(|id| json!({ "audio": { "id": id } })),
            Writing::Thinking => {
                let text = self.w.text(index).unwrap_or_default();
                Some(PartKind::Thinking.part([PartKind::Text.part(text)]))
            }
        };
        if let Some(item) = item {
            self.w.edit(index, |slot| *slot = item)?;
        }
        self.w.finish(index)
    }

    /// One content part, by its `type`: text (or a part with no `type` and
    /// a string `text`), a refusal's text, Mistral's thinking, an image, or
    /// a part with no shared meaning, kept as it came.
    fn part(&mut self, part: &Value) -> Result<(), Error> {
        let text = |key: &str| part.str(key).unwrap_or_default();
        match PartKind::of(part) {
            Some(PartKind::Text) => self.write(Writing::Text, text("text")),
            None if part.str("type").is_none() && part.str("text").is_some() => {
                self.write(Writing::Text, text("text"))
            }
            Some(PartKind::Refusal) => self.write(Writing::Text, text("refusal")),
            Some(PartKind::Thinking) => match part.get("thinking") {
                Some(Value::Array(chunks)) => {
                    let text: String = chunks
                        .iter()
                        .filter_map(|chunk| chunk.str("text"))
                        .collect();
                    self.write(Writing::Thinking, text)
                }
                _ => self.write(Writing::Thinking, text("thinking")),
            },
            Some(PartKind::ImageUrl) => self.image(part),
            _ => {
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
        let (media_type, source) = match url
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(";base64,"))
        {
            Some((media_type, data)) => (media_type.into(), Source::Data { data: data.into() }),
            None => (
                SmolStr::new_static("image/*"),
                Source::Url { url: url.into() },
            ),
        };
        self.close_writing()?;
        let index = self.w.fresh_index();
        let media = Media {
            media_type,
            source,
            meta: Meta::new(),
        };
        self.w.open(index, Open::Media(media), Value::Null)?;
        self.w.finish(index)
    }

    /// One tool-call fragment, `None` when it is no object. It goes to the
    /// call at its wire index (or `position`), where the writer tells a new
    /// id under it from the call it held. Without an index it goes to the
    /// open call that states its id; one that states no id continues the
    /// latest call while that call's arguments are not yet a whole object,
    /// and otherwise opens a new call when it brings a name or arguments.
    fn call(&mut self, call: Option<&Call<'_>>, position: Option<usize>) -> Result<(), Error> {
        self.close_writing()?;
        let id = call.and_then(|call| match &call.id {
            Json::Str(id) => Some(Cow::Borrowed(id.as_ref())),
            Json::Other(id @ Value::Number(_)) => Some(Cow::Owned(id.to_string())),
            _ => None,
        });
        let id = id.filter(|id| !id.is_empty() && id != "null");
        let function = call.and_then(|call| call.function.object());
        let name = match function.map(|function| &function.name) {
            None | Some(Json::Absent | Json::Other(Value::Null)) => call
                .and_then(|call| call.rest.get("custom")?.at("/name"))
                .and_then(Value::as_str),
            Some(name) => name.str(),
        };
        let arguments = function.and_then(|function| function.arguments.text());
        let arguments = arguments.as_deref();
        let starts = name.is_some_and(|name| !name.is_empty())
            || arguments.is_some_and(|text| !text.is_empty());
        let index = position.or_else(|| {
            let index = usize::try_from(call?.index.as_u64()?).ok()?;
            Some(index).filter(|index| *index < AUTO_INDEX)
        });
        let at = match index {
            Some(index) => index,
            None => match id.as_deref() {
                Some(id) => self
                    .calls
                    .iter()
                    .find(|open| open.id.as_deref() == Some(id)),
                None => self.calls.last().filter(|open| {
                    let arguments = self.w.text(open.at).unwrap_or_default();
                    open.opaque || !whole_object(arguments) || !starts
                }),
            }
            .map_or_else(|| self.w.fresh_index(), |open| open.at),
        };
        let opaque = if let Some(open) = self.calls.iter_mut().find(|open| open.at == at) {
            if let Some(id) = &id {
                open.id = Some(id.as_ref().to_owned());
            }
            open.opaque
        } else {
            let kind = call.map(|call| CallKind::of(call.kind.str()));
            let opaque = kind == Some(CallKind::Unknown);
            if opaque {
                self.w
                    .open(at, Open::Opaque { replay: false }, Value::Null)?;
            }
            self.calls.push(OpenCall {
                at,
                id: id.as_deref().map(str::to_owned),
                opaque,
            });
            opaque
        };
        if !opaque {
            let id = id.as_deref();
            let fragment = CallFragment {
                id,
                name,
                arguments,
            };
            self.w.fragment(at, fragment)?;
        }
        self.w.edit(at, |item| {
            if !item.is_object() {
                *item = Value::Object(Map::new());
            }
            if let (Value::Object(item), Some(call)) = (item, call) {
                call.merge(item);
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
                if CallKind::of(item.str("type")) == CallKind::Custom {
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

    /// Write the provider's end of the reply.
    fn end(&mut self) -> Result<Flow, Error> {
        self.close_writing()?;
        self.close_reasoning()?;
        let mut end = std::mem::take(&mut self.end);
        end.usage = (end.raw_usage.as_ref()).map_or_default(|usage| usage_of(usage, &self.quirks));
        end.reason = self.finish.take().map(|(reason, raw)| (reason, Some(raw)));
        self.w.end(end)
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
}

impl Writes for ChatDecoder {
    fn writer(&mut self) -> &mut Writer {
        &mut self.w
    }

    fn data(&mut self, data: &str) -> Result<Flow, Error> {
        // A host whose streams omit the finish reason ends a turn that
        // streamed a chunk at `[DONE]`: as tool calls when it holds one,
        // else as a stop.
        if data.trim() == "[DONE]" {
            if !self.ended && self.chunked && self.quirks.done_without_finish_reason {
                self.finish = Some(if self.calls.is_empty() {
                    (Reason::Stop, SmolStr::new_static("stop"))
                } else {
                    (Reason::ToolCalls, SmolStr::new_static("tool_calls"))
                });
                self.ended = true;
            }
            return self.finish();
        }
        let decode = |error| Error::decode(error, data);
        let value: Value;
        let frame: Json<'_, Frame<'_>, IgnoredAny> = if let Ok(frame) = serde_json::from_str(data) {
            frame
        } else {
            // A frame that repeats a key: as a `Value`, the last one stands.
            value = serde_json::from_str(data).map_err(decode)?;
            Json::deserialize(&value).map_err(decode)?
        };
        let frame = match frame {
            Json::Object(frame) => frame,
            // A bare string is the whole answer, ended as a stop.
            Json::Str(text) if self.quirks.bare_string_reply => {
                self.ended = true;
                self.finish = Some((Reason::Stop, SmolStr::new_static("stop")));
                self.write(Writing::Text, text)?;
                return self.end();
            }
            _ => return Ok(Flow::More),
        };
        if let Some(error) = failure(&frame, data) {
            return Err(error);
        }
        // Without a `type`, a frame is a chunk when it says so or carries
        // choices; anything else is not this wire's and is skipped. The
        // `object` tag decides when the host sends one: a stream chunk may
        // carry a whole message.
        let object = frame.object.str();
        if object != Some("chat.completion.chunk") && matches!(frame.choices, Json::Absent) {
            return Ok(Flow::More);
        }
        let whole = object.map_or_else(
            || {
                let mut choices = frame.choices.items().iter().filter_map(Json::object);
                choices.any(|choice| choice.message.object().is_some())
            },
            |object| object == "chat.completion",
        );
        let choice = self.absorb(frame);
        if whole {
            let Some(choice) = choice else {
                return Err(Error::decode("the reply holds no choices", data));
            };
            let Json::Object(Choice {
                message: Json::Object(message),
                ..
            }) = choice
            else {
                return Err(Error::decode("the reply holds no message", data));
            };
            self.ended = true;
            self.delta(message, true)?;
            self.close_calls(true)?;
            return self.end();
        }
        self.chunked = true;
        if let Some(Json::Object(Choice {
            delta: Json::Object(delta),
            ..
        })) = choice
        {
            self.delta(delta, false)?;
        }
        // Text and reasoning stay open until the stream ends: a provider may
        // still send either after the finish chunk.
        if matches!(self.finish, Some((Reason::ToolCalls, _))) {
            self.close_calls(false)?;
        }
        Ok(Flow::More)
    }

    /// A stream that stops after a finish reason without `[DONE]` still
    /// ended: some hosts (Perplexity) never send the sentinel.
    fn ended(&mut self) -> Result<(), Error> {
        self.finish().map(drop)
    }
}

/// The in-band error envelope, delivered with a 200 status. A `null` or
/// empty `error` can accompany valid terminal usage, and only a populated
/// `choices` array outranks the error.
fn failure(frame: &Frame<'_>, data: &str) -> Option<Error> {
    let error = &frame.error;
    if !(error.is_object() || error.as_str().is_some_and(|text| !text.is_empty()))
        || !frame.choices.items().is_empty()
    {
        return None;
    }
    let code = match error.get("code") {
        Some(Value::String(code)) => Some(code.clone()),
        Some(Value::Number(code)) => Some(code.to_string()),
        _ => error.str("type").map(str::to_owned),
    };
    let message = error.as_str().or_else(|| error.str("message"));
    let message = message.map_or_else(
        || serde_json::from_str::<Value>(data).map_or_default(|frame| frame.to_string()),
        str::to_owned,
    );
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
    let audio_input = count("/prompt_tokens_details/audio_tokens").filter(|tokens| *tokens > 0);
    let input = prompt.map(|prompt| {
        let beside = prompt.saturating_add(audio_input.unwrap_or(0));
        let accounted = beside.saturating_add(completion.unwrap_or(0));
        if audio_input.is_some() && Some(accounted) == total {
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
        audio_input,
        reasoning: detail("completion_tokens_details", "reasoning_tokens")
            .filter(|_| quirks.reliable_reasoning_count),
        audio_output: count("/completion_tokens_details/audio_tokens").filter(|tokens| *tokens > 0),
        ..Usage::default()
    }
}
