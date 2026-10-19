//! The decoder as it read frames before it read each once (a typed fast
//! path for deltas, a `Value` for any other frame), kept as the reference
//! the decoder must agree with event for event. Messages frames into
//! parts: each content block becomes one part, in wire
//! order, whose provider item is the block as the provider stated it
//! complete, and only when the wire would take that block back. Fields are
//! read leniently, so an invented field, or a known one of another type,
//! never fails a reply; a block the decoder cannot build from does.
//!
//! The bulk of a stream, a `content_block_delta` carrying one string, is
//! read into borrowed typed fields; any other frame goes through a `Value`.
//! Citations are not modeled: their deltas land on the text block's item,
//! which replays them.

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::KEY;
use crate::error::{Error, Kind};
use crate::json::{Lenient, Str, tag, whole_object};
use crate::protocol::Flow;
use crate::spec::{Reason, SmolStr, Usage};
use crate::writer::{AUTO_INDEX, CallFragment, End, Open, Writer, Writes, merge};

type Fields = Map<String, Value>;

/// The events the decoder reads; it skips any other. `message` is a whole
/// message, as a gateway may send in place of a stream.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EventType {
    Message,
    MessageStart,
    ContentBlockStart,
    ContentBlockDelta,
    ContentBlockStop,
    MessageDelta,
    Error,
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BlockType {
    Text,
    Thinking,
    RedactedThinking,
    ToolUse,
    /// The model that took over the request.
    Fallback,
    /// Any other block: an opaque part.
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DeltaType {
    TextDelta,
    ThinkingDelta,
    InputJsonDelta,
    SignatureDelta,
    CitationsDelta,
    #[serde(other)]
    Other,
}

impl DeltaType {
    /// The wire name and the string field of a delta that carries one.
    const fn field(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::TextDelta => Some(("text_delta", "text")),
            Self::ThinkingDelta => Some(("thinking_delta", "thinking")),
            Self::InputJsonDelta => Some(("input_json_delta", "partial_json")),
            Self::SignatureDelta => Some(("signature_delta", "signature")),
            Self::CitationsDelta | Self::Other => None,
        }
    }
}

/// How a `stop_reason` ends the turn. `pause_turn` is a server-tool loop
/// that stopped at its limit: the turn is replayed as it is to resume it.
fn stop_reason(stop: &str) -> Reason {
    match stop {
        "end_turn" | "stop_sequence" | "pause_turn" => Reason::Stop,
        "max_tokens" | "model_context_window_exceeded" => Reason::Length,
        "tool_use" => Reason::ToolCalls,
        "refusal" => Reason::ContentFilter,
        _ => Reason::Other,
    }
}

/// A `content_block_delta` as nearly every token streams: one string beside
/// the delta's type, borrowed from the frame unless it is escaped.
#[derive(Deserialize)]
pub(super) struct Fast<'a> {
    #[serde(rename = "type")]
    event: EventType,
    index: usize,
    #[serde(borrow)]
    delta: FastDelta<'a>,
}

/// Any other field sends the delta the `Value` way, which merges it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FastDelta<'a> {
    #[serde(rename = "type")]
    kind: DeltaType,
    #[serde(borrow)]
    text: Option<Str<'a>>,
    #[serde(borrow)]
    thinking: Option<Str<'a>>,
    #[serde(borrow)]
    partial_json: Option<Str<'a>>,
    #[serde(borrow)]
    signature: Option<Str<'a>>,
}

impl<'a> Fast<'a> {
    /// The block, type and fragment of a delta stating exactly the field
    /// its type names.
    pub(super) fn read(data: &'a str) -> Option<(usize, DeltaType, Cow<'a, str>)> {
        let Self {
            event,
            index,
            delta,
        } = serde_json::from_str(data).ok()?;
        let fragments = [
            delta.text,
            delta.thinking,
            delta.partial_json,
            delta.signature,
        ];
        let fragment = match (delta.kind, fragments) {
            (DeltaType::TextDelta, [Some(text), None, None, None])
            | (DeltaType::ThinkingDelta, [None, Some(text), None, None])
            | (DeltaType::InputJsonDelta, [None, None, Some(text), None])
            | (DeltaType::SignatureDelta, [None, None, None, Some(text)]) => text.0,
            _ => return None,
        };
        let streamed = matches!(event, EventType::ContentBlockDelta) && index < AUTO_INDEX;
        streamed.then_some((index, delta.kind, fragment))
    }
}

/// The reply's usage from the terminal counters, read leniently, with
/// `start`'s (`message_start`'s) for the input and cache counts they omit.
/// A zero input count is a gateway's placeholder; zero cache counts are
/// counts. The input is `input_tokens` plus the cache reads and writes
/// Anthropic counts beside it; the output includes thinking.
fn usage(terminal: Option<&Value>, start: Option<&Value>) -> Usage {
    let count = |usage: Option<&Value>, pointer: &str| usage?.at(pointer)?.as_u64_lenient();
    let either = |pointer| count(terminal, pointer).or_else(|| count(start, pointer));
    let input = count(terminal, "/input_tokens").filter(|tokens| *tokens > 0);
    let input = input.or_else(|| count(start, "/input_tokens"));
    let cache_read = either("/cache_read_input_tokens");
    let cache_write = either("/cache_creation_input_tokens");
    let cached = cache_read.unwrap_or(0) + cache_write.unwrap_or(0);
    let server_tools = terminal.and_then(|usage| usage.get("server_tool_use"));
    Usage {
        input: input.map(|uncached| uncached + cached),
        output: count(terminal, "/output_tokens"),
        cache_read,
        cache_write,
        cache_write_long: either("/cache_creation/ephemeral_1h_input_tokens"),
        reasoning: count(terminal, "/output_tokens_details/thinking_tokens"),
        hosted_tool_calls: crate::json::counters(server_tools, None).filter(|calls| *calls > 0),
        ..Usage::default()
    }
}

/// `content_block_stop` states a block complete; a stop reason states every
/// block still open complete, but a call whose input is not yet JSON.
pub(crate) struct Oracle {
    w: Writer,
    /// Each open block by index, in index order: its type (a fallback's as
    /// `Other`) and the input JSON streamed to it.
    open: Vec<(usize, BlockType, String)>,
    /// Whether a block other than a leading `fallback` opened.
    opened: bool,
    unsigned_thinking: bool,
    /// The message's id and model, and its usage object: `message_start`'s,
    /// then with the terminal delta's over it.
    end: End,
    container: Option<Value>,
}

/// The block or delta `value`, which must name its string `type`.
fn typed<'v, T: Deserialize<'v>>(
    value: Option<&'v Value>,
    data: &str,
    missing: impl FnOnce() -> String,
) -> Result<(T, &'v Fields), Error> {
    let read = |item: &'v Fields| Some((tag(item.get("type"))?, item));
    let read = value.and_then(Value::as_object).and_then(read);
    read.ok_or_else(|| Error::decode(missing(), data))
}

/// `fragment` appended to the string `item` holds at `key`, as [`merge`]
/// merges a delta of that one field.
fn append(item: &mut Value, key: &str, fragment: &str) {
    if !item.is_object() {
        *item = json!({});
    }
    match item.get_mut(key) {
        Some(Value::String(text)) => text.push_str(fragment),
        _ => item[key] = fragment.into(),
    }
}

impl Oracle {
    /// `unsigned_thinking`: whether the host takes thinking back unsigned.
    pub(crate) fn new(unsigned_thinking: bool) -> Self {
        Self {
            w: Writer::new(KEY).with_call_id_slot(),
            open: Vec::new(),
            opened: false,
            unsigned_thinking,
            end: End::default(),
            container: None,
        }
    }

    /// Open the content block at `index` as the provider states it.
    fn start(&mut self, index: usize, kind: BlockType, block: &Fields) -> Result<(), Error> {
        let fallback = kind == BlockType::Fallback;
        // A leading `fallback` names the model that took over; one after
        // output began is a fallback no part can represent (pi's rule).
        if fallback && self.opened {
            return Err(Error::new(
                Kind::Other,
                "Anthropic performed an unsupported mid-output model fallback",
            ));
        }
        self.opened |= !fallback;
        let held = if fallback { BlockType::Other } else { kind };
        let at = self.open.partition_point(|(at, ..)| *at < index);
        match self.open.get_mut(at) {
            Some(slot) if slot.0 == index => *slot = (index, held, String::new()),
            _ => self.open.insert(at, (index, held, String::new())),
        }
        let block = Value::Object(block.clone());
        let (open, text) = match kind {
            BlockType::Text => (Open::Text, block.str("text")),
            BlockType::Thinking => (Open::Reasoning { redacted: false }, block.str("thinking")),
            BlockType::RedactedThinking => (Open::Reasoning { redacted: true }, None),
            BlockType::ToolUse => return self.call(index, block),
            BlockType::Fallback | BlockType::Other => (Open::Opaque { replay: !fallback }, None),
        };
        let text = text.unwrap_or_default().to_owned();
        self.w.open(index, open, block)?;
        self.w.push(index, text)
    }

    /// Open a `tool_use` block. A whole reply states the input on the
    /// block; a stream streams it, and the fragments win.
    fn call(&mut self, index: usize, block: Value) -> Result<(), Error> {
        let id: SmolStr = block.str("id").unwrap_or_default().into();
        let input = block.get("input").cloned().unwrap_or_default();
        if let Some(name) = block.str("name").filter(|name| !name.is_empty()) {
            let name = name.into();
            self.w.open(index, Open::Call { id, name }, block)?;
        } else {
            // A nameless call, which the writer drops.
            let id = Some(id.as_str());
            let fragment = CallFragment {
                id,
                ..CallFragment::default()
            };
            self.w.fragment(index, fragment)?;
        }
        self.w.announce(index, input)
    }

    /// Apply a delta to the block at `index`: text and thinking grow the
    /// part, input JSON is assembled, and every delta but input JSON then
    /// lands on the item, as the whole `delta` object where one was read
    /// (through [`merge`]), else as its one string field. `fragment` is the
    /// string field [`DeltaType::field`] names.
    fn delta(
        &mut self,
        index: usize,
        kind: DeltaType,
        fragment: Option<Cow<'_, str>>,
        delta: Option<&Fields>,
        data: &str,
    ) -> Result<(), Error> {
        let (wire, key) = kind.field().unwrap_or_default();
        let land = |item: &mut Value, fragment: &str| match (delta, item) {
            (None, item) => append(item, key, fragment),
            (Some(delta), Value::Object(item)) if kind == DeltaType::CitationsDelta => {
                let citation = delta.get("citation").cloned().unwrap_or_default();
                match item.get_mut("citations") {
                    Some(Value::Array(citations)) => citations.push(citation),
                    _ => {
                        item.insert("citations".into(), json!([citation]));
                    }
                }
            }
            (Some(_), _) if kind == DeltaType::CitationsDelta => {}
            // Text and signatures concatenate; deltas never seen before
            // land in the item too.
            (Some(delta), item) => merge(item, delta),
        };
        // The fragment a block is built from must be text.
        let missing = || {
            let missing = format!("an Anthropic `{wire}` carries no string `{key}`");
            Error::decode(missing, data)
        };
        let opens = match kind {
            DeltaType::TextDelta => BlockType::Text,
            DeltaType::ThinkingDelta => BlockType::Thinking,
            DeltaType::InputJsonDelta => {
                let fragment = fragment.ok_or_else(missing)?;
                let open = self.open.iter_mut().find(|(at, ..)| *at == index);
                let Some((_, kind @ (BlockType::ToolUse | BlockType::Other), json)) = open else {
                    let takes_none = format!(
                        "Anthropic streamed input to content block {index}, which takes none"
                    );
                    return Err(Error::decode(takes_none, data));
                };
                json.push_str(&fragment);
                if *kind == BlockType::ToolUse {
                    self.w.push(index, fragment)?;
                }
                return Ok(());
            }
            _ => {
                let fragment = fragment.as_deref().unwrap_or_default();
                return self.w.edit(index, |item| land(item, fragment));
            }
        };
        // A gateway that skips `content_block_start` still streams the
        // block's text, so its first delta opens it.
        if !self.w.opened(index) {
            let block = crate::json::object([("type".into(), key.into()), (key.into(), "".into())]);
            self.start(index, opens, &block)?;
        }
        let fragment = fragment.ok_or_else(missing)?;
        self.w.edit(index, |item| land(item, &fragment))?;
        self.w.push(index, fragment)
    }

    /// End the block at `index` as stated complete. Its item becomes the
    /// part's state only when the wire takes it back: text that is not
    /// blank, thinking with its signature (unless the host takes it
    /// unsigned), redacted thinking with its data, and a call with an
    /// object `input`, set from what streamed. A hosted item whose input is
    /// not an object never completed.
    fn stop(&mut self, index: usize) -> Result<(), Error> {
        let Some(at) = self.open.iter().position(|(at, ..)| *at == index) else {
            return self.w.finish(index);
        };
        let (_, kind, json) = self.open.remove(at);
        let call = kind == BlockType::ToolUse;
        let unsigned = self.unsigned_thinking;
        let (mut complete, mut arguments) = (true, None);
        self.w.edit(index, |item| {
            let stated = |key| item.str(key).unwrap_or_default();
            let kept = match kind {
                BlockType::Text => !stated("text").trim().is_empty(),
                BlockType::Thinking => unsigned || !stated("signature").is_empty(),
                BlockType::RedactedThinking => !stated("data").is_empty(),
                BlockType::ToolUse | BlockType::Fallback | BlockType::Other => {
                    // What streamed, `None` if it is no JSON, else what the
                    // block states.
                    let input = match json.trim() {
                        "" if json.is_empty() => Some(item.get("input").cloned()),
                        "" => Some(Some(json!({}))),
                        json => serde_json::from_str(json).ok().map(Some),
                    };
                    let input = match (input, call) {
                        (Some(Some(input @ Value::Object(_))), _) => Some(input),
                        (Some(None | Some(Value::Null)), true) => Some(json!({})),
                        (Some(None), false) => None,
                        // Input stated on the block that is no object stands
                        // as the call's argument text, which the writer
                        // judges.
                        (Some(Some(stated)), true) if json.is_empty() => {
                            arguments = Some(stated.to_string());
                            return *item = Value::Null;
                        }
                        (_, true) => return *item = Value::Null,
                        (_, false) => {
                            complete = false;
                            None
                        }
                    };
                    if let (Some(input), Some(fields)) = (input, item.as_object_mut()) {
                        fields.insert("input".into(), input);
                    }
                    true
                }
            };
            if !kept {
                *item = Value::Null;
            }
        })?;
        if let Some(arguments) = arguments {
            self.w.push(index, &arguments)?;
        }
        if complete {
            self.w.finish(index)
        } else {
            self.w.close(index)
        }
    }

    fn metadata(&mut self, message: &Value) {
        let usage = message.get("usage");
        self.end.raw_usage = usage.filter(|usage| usage.is_object()).cloned();
        self.end.response_id = message.str("id").map(Into::into);
        self.end.model = message.str("model").map(Into::into);
        self.note_container(message.get("container"));
    }

    fn note_container(&mut self, container: Option<&Value>) {
        if let Some(container) = container.filter(|container| !container.is_null()) {
            self.container = Some(container.clone());
        }
    }

    /// End the reply. Every block still open is complete, but a call whose
    /// input is not JSON yet: the end of the reply closes it unfinished. The
    /// container the reply ran in is a last opaque part.
    fn end(
        &mut self,
        usage: Usage,
        stop: Option<&str>,
        details: Option<&Value>,
    ) -> Result<Flow, Error> {
        let open = self.open.iter();
        let open = open.filter(|(_, kind, json)| *kind != BlockType::ToolUse || whole_object(json));
        for index in open.map(|(index, ..)| *index).collect::<Vec<_>>() {
            self.stop(index)?;
        }
        if let Some(container) = self.container.take() {
            let index = self.w.fresh_index();
            let item = json!({"type": "container", "container": container});
            self.w
                .whole(index, Open::Opaque { replay: true }, item, "")?;
        }
        let reason = stop.map(|stop| (stop_reason(stop), Some(stop.into())));
        let message = matches!(reason, Some((Reason::ContentFilter, _))).then(|| {
            let explanation = details.and_then(|details| details.str("explanation"));
            let explanation = explanation.filter(|explanation| !explanation.is_empty());
            explanation
                .unwrap_or("The model refused to complete the request")
                .to_owned()
        });
        self.w.end(End {
            usage,
            reason,
            message,
            ..std::mem::take(&mut self.end)
        })
    }

    /// A whole message, written block by block as a stream writes it, then
    /// ended. Empty content is a turn like any other: the stop reason
    /// decides how it ends.
    fn whole(&mut self, message: &Value, data: &str) -> Result<Flow, Error> {
        self.metadata(message);
        for (index, block) in message.arr("content").iter().enumerate() {
            let missing = || format!("Anthropic content block {index} has no string `type`");
            let (kind, block) = typed(Some(block), data, missing)?;
            self.start(index, kind, block)?;
            self.stop(index)?;
        }
        let stop = message.str("stop_reason");
        let usage = usage(message.get("usage"), message.get("usage"));
        self.end(usage, stop, message.get("stop_details"))
    }

    /// Decode a frame through a `Value`.
    pub(super) fn value(&mut self, data: &str) -> Result<Flow, Error> {
        let frame: Value =
            serde_json::from_str(data).map_err(|error| Error::decode(error, data))?;
        let index = || {
            let index = frame
                .u64("index")
                .and_then(|index| usize::try_from(index).ok());
            let index = index.filter(|index| *index < AUTO_INDEX);
            index.ok_or_else(|| Error::decode("an Anthropic event names no block index", data))
        };
        let missing =
            |key| move || format!("an Anthropic event carries no `{key}` with a string `type`");
        match tag(frame.get("type")) {
            Some(EventType::Message) => return self.whole(&frame, data),
            // A `message_start` without a message (a Bedrock-compatible
            // gateway sends one) says nothing.
            Some(EventType::MessageStart) => {
                if let Some(message) = frame.get("message").filter(|message| message.is_object()) {
                    self.metadata(message);
                }
            }
            Some(EventType::ContentBlockStart) => {
                let (kind, block) =
                    typed(frame.get("content_block"), data, missing("content_block"))?;
                self.start(index()?, kind, block)?;
            }
            Some(EventType::ContentBlockDelta) => {
                let (kind, delta) = typed::<DeltaType>(frame.get("delta"), data, missing("delta"))?;
                let fragment = kind.field().and_then(|(_, key)| delta.get(key)?.as_str());
                self.delta(
                    index()?,
                    kind,
                    fragment.map(Cow::Borrowed),
                    Some(delta),
                    data,
                )?;
            }
            Some(EventType::ContentBlockStop) => self.stop(index()?)?,
            Some(EventType::MessageDelta) => {
                let delta = frame.get("delta");
                self.note_container(delta.and_then(|delta| delta.get("container")));
                // Only a `message_delta` with a stop reason ends the reply.
                let Some(stop) = delta.and_then(|delta| delta.str("stop_reason")) else {
                    return Ok(Flow::More);
                };
                let usage = usage(frame.get("usage"), self.end.raw_usage.as_ref());
                if let Some(delta) = frame.obj("usage")
                    && let Value::Object(raw) = self.end.raw_usage.get_or_insert_with(|| json!({}))
                {
                    raw.extend(delta.clone());
                }
                let details = delta.and_then(|delta| delta.get("stop_details"));
                return self.end(usage, Some(stop), details);
            }
            Some(EventType::Error) => {
                let error = frame.get("error").unwrap_or(&frame);
                let message = error
                    .str("message")
                    .map_or_else(|| frame.to_string(), str::to_owned);
                return Err(Error::provider(error.str("type"), message));
            }
            Some(EventType::Other) | None => {}
        }
        Ok(Flow::More)
    }
}

impl Writes for Oracle {
    fn writer(&mut self) -> &mut Writer {
        &mut self.w
    }

    fn data(&mut self, data: &str) -> Result<Flow, Error> {
        if data.trim() == "[DONE]" {
            // The provider's end is the `message_delta` with a stop reason.
            return Ok(Flow::More);
        }
        let Some((index, kind, fragment)) = Fast::read(data) else {
            return self.value(data);
        };
        self.delta(index, kind, Some(fragment), None, data)?;
        Ok(Flow::More)
    }
}
