//! Messages frames into parts: each content block becomes one part, in wire
//! order, whose provider item is the block as the provider stated it
//! complete, and only when the wire would take that block back. Fields are
//! read leniently, so an invented field, or a known one of another type,
//! never fails a reply; a block the decoder cannot build from does.
//!
//! A frame is read once, into fields that borrow its strings: a delta,
//! nearly every frame, builds no tree, and the text or thinking a block
//! streams lands on its item once, when the block stops, rather than with
//! each token. A block's item, the message and the usage are trees: the
//! item is what the provider sends back, so every field it states stays.
//! Citations are not modeled: their deltas land on the text block's item,
//! which replays them.

use std::borrow::Cow;

use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use super::KEY;
use crate::error::{Error, Kind};
use crate::json::{Count, Fielded, FromFields, Json, Lenient, Word, tag, whole_object};
use crate::protocol::Flow;
use crate::spec::{Reason, SmolStr, Usage};
use crate::writer::{AUTO_INDEX, CallFragment, End, Open, Writer, Writes};

type Object = Map<String, Value>;

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

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BlockType {
    Text,
    Thinking,
    RedactedThinking,
    ToolUse,
    /// The model that took over the request.
    Fallback,
    /// Any other block: an opaque part.
    #[default]
    #[serde(other)]
    Other,
}

impl BlockType {
    /// The string field of its item a block's own deltas extend.
    const fn grows(self) -> Option<&'static str> {
        match self {
            Self::Text => Some("text"),
            Self::Thinking => Some("thinking"),
            _ => None,
        }
    }
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

/// How each `stop_reason` ends the turn. `pause_turn` is a server-tool
/// loop that stopped at its limit: the turn is replayed as it is to resume
/// it.
const STOPS: &[(&str, Reason)] = &[
    ("end_turn", Reason::Stop),
    ("stop_sequence", Reason::Stop),
    ("pause_turn", Reason::Stop),
    ("max_tokens", Reason::Length),
    ("model_context_window_exceeded", Reason::Length),
    ("tool_use", Reason::ToolCalls),
    ("refusal", Reason::ContentFilter),
];

/// An event's fields, read whatever its type: the type may come last. A
/// block, a message and a usage object stay text until their event reads
/// them, so a delta's frame holds no tree.
#[derive(Default)]
pub(super) struct Frame<'a> {
    kind: Word<EventType>,
    index: Count,
    pub(super) delta: Fielded<Delta<'a>>,
    content_block: Option<&'a RawValue>,
    message: Option<&'a RawValue>,
    usage: Option<&'a RawValue>,
}

/// `raw` as a tree, `null` when absent.
fn tree(raw: Option<&RawValue>) -> Value {
    raw.and_then(|raw| serde_json::from_str(raw.get()).ok())
        .unwrap_or_default()
}

/// An event's keys, read as a vocabulary: an event is a frame.
#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum FrameKey {
    Type,
    Index,
    Delta,
    ContentBlock,
    Message,
    Usage,
    #[serde(other)]
    Other,
}

impl<'de: 'a, 'a> FromFields<'de> for Frame<'a> {
    fn read<A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut frame = Self::default();
        while let Some(key) = map.next_key()? {
            match key {
                FrameKey::Type => frame.kind = map.next_value()?,
                FrameKey::Index => frame.index = map.next_value()?,
                FrameKey::Delta => frame.delta = map.next_value()?,
                FrameKey::ContentBlock => frame.content_block = map.next_value()?,
                FrameKey::Message => frame.message = map.next_value()?,
                FrameKey::Usage => frame.usage = map.next_value()?,
                FrameKey::Other => drop(map.next_value::<IgnoredAny>()?),
            }
        }
        Ok(frame)
    }
}

/// A `content_block_delta`'s delta or a `message_delta`'s: its type, the
/// string a text, thinking or input delta streams, borrowed, and every
/// other field as it came. The type may follow the string, so the string
/// is held under its key until the type names the one it streams.
#[derive(Default)]
pub(super) struct Delta<'a> {
    kind: Word<DeltaType>,
    pub(super) streamed: Option<(&'static str, Cow<'a, str>)>,
    rest: Object,
}

/// A delta's keys: those it reads, and any other by name.
#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum DeltaKey {
    Type,
    Text,
    Thinking,
    PartialJson,
    Other(String),
}

impl<'de: 'a, 'a> FromFields<'de> for Delta<'a> {
    fn read<A: MapAccess<'de>>(mut map: A) -> Result<Self, A::Error> {
        let mut delta = Self::default();
        while let Some(key) = map.next_key()? {
            match key {
                DeltaKey::Type => delta.kind = map.next_value()?,
                DeltaKey::Text => delta.hold("text", map.next_value()?),
                DeltaKey::Thinking => delta.hold("thinking", map.next_value()?),
                DeltaKey::PartialJson => delta.hold("partial_json", map.next_value()?),
                // A field stated twice reads as its last value.
                DeltaKey::Other(key) => drop(delta.rest.insert(key, map.next_value()?)),
            }
        }
        Ok(delta)
    }
}

impl<'a> Delta<'a> {
    /// A string field a delta may stream, held, and any other value of it
    /// among the fields. A string held under another key joins the fields.
    fn hold(&mut self, key: &'static str, json: Json<'a>) {
        if let Some((held, text)) = self.streamed.take_if(|(held, _)| *held != key) {
            self.rest.insert(held.to_owned(), text.into_owned().into());
        }
        match json {
            Json::Str(text) => {
                self.rest.remove(key);
                self.streamed = Some((key, text));
            }
            json => {
                self.streamed = None;
                let value = match json {
                    Json::Object(object) => object,
                    json => json.into_value().unwrap_or_default(),
                };
                self.rest.insert(key.to_owned(), value);
            }
        }
    }

    /// The string its type streams to its block, and its other fields but
    /// its type, which land on the block's item.
    fn split(mut self) -> (Option<Cow<'a, str>>, Object) {
        let key = match self.kind.0 {
            Some(DeltaType::TextDelta) => "text",
            Some(DeltaType::ThinkingDelta) => "thinking",
            Some(DeltaType::InputJsonDelta) => "partial_json",
            _ => "",
        };
        let mut fragment = None;
        if let Some((held, text)) = self.streamed {
            if held == key {
                fragment = Some(text);
            } else {
                self.rest.insert(held.to_owned(), text.into_owned().into());
            }
        }
        // A string another one followed under another key is a field.
        if fragment.is_none() && matches!(self.rest.get(key), Some(Value::String(_))) {
            fragment = self.rest.remove(key).and_then(|text| match text {
                Value::String(text) => Some(text.into()),
                _ => None,
            });
        }
        (fragment, self.rest)
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

/// A content block while it is open: the decoder's state in the writer's
/// draft of it.
#[derive(Default)]
pub(crate) struct Block {
    /// Its type, a fallback's as `Other`.
    kind: BlockType,
    /// The input JSON streamed to it.
    json: String,
    depth: crate::json::JsonDepth,
    /// Whether its own deltas extended its text, which its item takes when
    /// it stops: the writer holds that text already.
    grew: bool,
    /// Whether its item takes deltas as they come: once a delta extends
    /// another field, or one of its text's, the item's text is no longer
    /// the writer's.
    eager: bool,
}

/// `content_block_stop` states a block complete; a stop reason states every
/// block still open complete, but a call whose input is not yet JSON.
pub(crate) struct MessagesDecoder {
    w: Writer<Block>,
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
) -> Result<(T, &'v Object), Error> {
    let read = |item: &'v Object| Some((tag(item.get("type"))?, item));
    let read = value.and_then(Value::as_object).and_then(read);
    read.ok_or_else(|| Error::decode(missing(), data))
}

fn missing(key: &str) -> impl FnOnce() -> String {
    move || format!("an Anthropic event carries no `{key}` with a string `type`")
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

/// The block index an event names, below the writer's own.
fn index_of(index: &Count, data: &str) -> Result<usize, Error> {
    let index = index.0.and_then(|index| usize::try_from(index).ok());
    let index = index.filter(|index| *index < AUTO_INDEX);
    index.ok_or_else(|| Error::decode("an Anthropic event names no block index", data))
}

impl MessagesDecoder {
    /// `unsigned_thinking`: whether the host takes thinking back unsigned.
    pub(crate) fn new(unsigned_thinking: bool) -> Self {
        Self {
            w: Writer::new(KEY).with_call_id_slot(),
            opened: false,
            unsigned_thinking,
            end: End::default(),
            container: None,
        }
    }

    fn block(&mut self, index: usize) -> Option<&mut Block> {
        self.w.state(index)
    }

    /// Open the content block at `index` as the provider states it.
    fn start(&mut self, index: usize, kind: BlockType, block: &Object) -> Result<(), Error> {
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
        // A block opened again over one still open: the item it held takes
        // its text, as it took each token.
        self.settle(index)?;
        let held = Block {
            kind: if fallback { BlockType::Other } else { kind },
            ..Block::default()
        };
        let bytes = self.w.measure_fields(block)?;
        self.w.reserve(bytes)?;
        let block = Value::Object(block.clone());
        let (open, text) = match kind {
            BlockType::Text => (Open::Text, block.str("text")),
            BlockType::Thinking => (Open::Reasoning { redacted: false }, block.str("thinking")),
            BlockType::RedactedThinking => (Open::Reasoning { redacted: true }, None),
            BlockType::ToolUse => return self.call(index, block, held),
            BlockType::Fallback | BlockType::Other => (Open::Opaque { replay: !fallback }, None),
        };
        let text = text.unwrap_or_default().to_owned();
        self.w.open(index, open, block)?;
        if let Some(state) = self.w.state(index) {
            *state = held;
        }
        self.w.push(index, text)
    }

    /// Open a `tool_use` block. A whole reply states the input on the
    /// block; a stream streams it, and the fragments win.
    fn call(&mut self, index: usize, block: Value, held: Block) -> Result<(), Error> {
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
        if let Some(state) = self.w.state(index) {
            *state = held;
        }
        self.w.announce(index, input)
    }

    /// The text the open block at `index` grew by, which its item takes:
    /// from now on the item takes each delta as it comes.
    fn settle(&mut self, index: usize) -> Result<(), Error> {
        let Some(block) = self.block(index).filter(|block| !block.eager) else {
            return Ok(());
        };
        block.eager = true;
        let grown = block.grew.then_some(block.kind.grows()).flatten();
        let Some(key) = grown else {
            return Ok(());
        };
        self.w.land(index, key)
    }

    /// Apply a delta to the block at `index`: text and thinking grow the
    /// part, input JSON is assembled, and every delta but input JSON lands
    /// on the item, as [`merge`] merges it (a citation joins the item's
    /// list). Text a block's own deltas stream waits for it to stop.
    fn delta(
        &mut self,
        index: usize,
        kind: DeltaType,
        delta: Delta<'_>,
        data: &str,
    ) -> Result<(), Error> {
        let (wire, key) = kind.field().unwrap_or_default();
        // A signature lands on the item as any other field does.
        let (fragment, mut others) = delta.split();
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
                let limits = self.w.limits();
                let held = self.block(index).map_or(0, |block| block.json.len());
                if fragment.len() > limits.argument_bytes.saturating_sub(held) {
                    return Err(Error::limit("argument bytes"));
                }
                self.w.reserve(fragment.len())?;
                let open = self.block(index);
                let Some(Block {
                    kind: kind @ (BlockType::ToolUse | BlockType::Other),
                    json,
                    depth,
                    ..
                }) = open
                else {
                    let takes_none = format!(
                        "Anthropic streamed input to content block {index}, which takes none"
                    );
                    return Err(Error::decode(takes_none, data));
                };
                depth.push(&fragment, limits.depth)?;
                json.push_str(&fragment);
                if *kind == BlockType::ToolUse {
                    self.w.push(index, fragment)?;
                }
                return Ok(());
            }
            DeltaType::CitationsDelta => {
                let citation = others.remove("citation").unwrap_or_default();
                let growth = self.w.measure_at(&citation, 2)?.saturating_add(16);
                return self.w.edit(index, growth, |item| {
                    let Value::Object(item) = item else {
                        return;
                    };
                    match item.get_mut("citations") {
                        Some(Value::Array(citations)) => citations.push(citation),
                        _ => {
                            item.insert("citations".into(), json!([citation]));
                        }
                    }
                });
            }
            DeltaType::SignatureDelta | DeltaType::Other => {
                self.land(index, &others)?;
                return self.w.merge(index, &others);
            }
        };
        // Nearly every frame: a block's own text, which waits for its stop.
        if let Some(block) = self.block(index)
            && !block.eager
            && block.kind.grows() == Some(key)
            && others.is_empty()
        {
            block.grew = true;
            return self.w.push(index, fragment.ok_or_else(missing)?);
        }
        // A gateway that skips `content_block_start` still streams the
        // block's text, so its first delta opens it.
        if !self.w.opened(index) {
            let block = crate::json::object([("type".into(), key.into()), (key.into(), "".into())]);
            self.start(index, opens, &block)?;
        }
        let fragment = fragment.ok_or_else(missing)?;
        let own = (self.block(index)).is_some_and(|block| block.kind.grows() == Some(key));
        self.land(index, &others)?;
        match self.block(index) {
            Some(block) if own && !block.eager => block.grew = true,
            _ => {
                self.settle(index)?;
                let growth = self
                    .w
                    .measure_str(&fragment)?
                    .saturating_add(key.len())
                    .saturating_add(4);
                self.w
                    .edit(index, growth, |item| append(item, key, &fragment))?;
                self.w.merge(index, &others)?;
                return self.w.push(index, fragment);
            }
        }
        if !others.is_empty() {
            self.w.merge(index, &others)?;
        }
        self.w.push(index, fragment)
    }

    /// Before `fields` land on the item at `index`: when they extend the
    /// text the item waits for, it takes that text first.
    fn land(&mut self, index: usize, fields: &Object) -> Result<(), Error> {
        let waits = self.block(index).and_then(|block| block.kind.grows());
        match waits {
            Some(key) if fields.contains_key(key) => self.settle(index),
            _ => Ok(()),
        }
    }

    /// End the block at `index` as stated complete. Its item becomes the
    /// part's state only when the wire takes it back: text that is not
    /// blank, thinking with its signature (unless the host takes it
    /// unsigned), redacted thinking with its data, and a call with an
    /// object `input`, set from what streamed. A hosted item whose input is
    /// not an object never completed.
    fn stop(&mut self, index: usize) -> Result<(), Error> {
        if self.block(index).is_none() {
            return self.w.finish(index);
        }
        self.settle(index)?;
        let Block { kind, json, .. } = self.block(index).map_or_default(std::mem::take);
        let limits = self.w.limits();
        crate::json::check_text(&json, limits.argument_bytes, limits.depth.saturating_sub(1))?;
        let call = kind == BlockType::ToolUse;
        let unsigned = self.unsigned_thinking;
        let (mut complete, mut arguments) = (true, None);
        self.w
            .transform(index, json.len().saturating_add(16), |item| {
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
        let open = self.w.open_states();
        let open =
            open.filter(|(_, block)| block.kind != BlockType::ToolUse || whole_object(&block.json));
        let mut open: Vec<usize> = open.map(|(index, _)| index).collect();
        open.sort_unstable();
        for index in open {
            self.stop(index)?;
        }
        if let Some(container) = self.container.take() {
            let index = self.w.fresh_index();
            let item = json!({"type": "container", "container": container});
            self.w
                .whole(index, Open::Opaque { replay: true }, item, "")?;
        }
        let reason = stop.map(|stop| (Reason::of(STOPS, stop), Some(stop.into())));
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

    /// The `message_delta` that ends the reply, once it states a stop
    /// reason.
    fn message_delta(&mut self, frame: Frame<'_>) -> Result<Flow, Error> {
        let mut delta = frame.delta.0.map_or_default(|delta| delta.rest);
        self.note_container(delta.get("container"));
        // Only a `message_delta` with a stop reason ends the reply.
        let Some(stop) = delta
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Ok(Flow::More);
        };
        let terminal = tree(frame.usage);
        let usage = usage(Some(&terminal), self.end.raw_usage.as_ref());
        if let Value::Object(delta) = terminal
            && let Value::Object(raw) = self.end.raw_usage.get_or_insert_with(|| json!({}))
        {
            raw.extend(delta);
        }
        let details = delta.remove("stop_details");
        self.end(usage, Some(&stop), details.as_ref())
    }
}

/// The error an `error` event reports.
fn failure(data: &str) -> Error {
    let frame: Value = serde_json::from_str(data).unwrap_or_default();
    let error = frame.get("error").unwrap_or(&frame);
    let message = error
        .str("message")
        .map_or_else(|| frame.to_string(), str::to_owned);
    Error::provider(error.str("type"), message)
}

/// `data` read as a frame; `None` for JSON that is no object.
pub(super) fn frame(data: &str) -> Result<Option<Frame<'_>>, Error> {
    let frame: Fielded<Frame<'_>> =
        serde_json::from_str(data).map_err(|error| Error::decode(error, data))?;
    Ok(frame.0)
}

impl Writes for MessagesDecoder {
    type State = Block;

    fn writer(&mut self) -> &mut Writer<Block> {
        &mut self.w
    }

    fn data(&mut self, data: &str) -> Result<Flow, Error> {
        if data.trim() == "[DONE]" {
            // The provider's end is the `message_delta` with a stop reason.
            return Ok(Flow::More);
        }
        let Some(frame) = frame(data)? else {
            return Ok(Flow::More);
        };
        let index = || index_of(&frame.index, data);
        match frame.kind.0 {
            Some(EventType::Message) => {
                let message: Value = serde_json::from_str(data).unwrap_or_default();
                return self.whole(&message, data);
            }
            // A `message_start` without a message (a Bedrock-compatible
            // gateway sends one) says nothing.
            Some(EventType::MessageStart) => {
                let message = tree(frame.message);
                if message.is_object() {
                    self.metadata(&message);
                }
            }
            Some(EventType::ContentBlockStart) => {
                let block = tree(frame.content_block);
                let (kind, block) = typed(Some(&block), data, missing("content_block"))?;
                self.start(index()?, kind, block)?;
            }
            Some(EventType::ContentBlockDelta) => {
                let delta = frame.delta.0.unwrap_or_default();
                let Some(kind) = delta.kind.0 else {
                    return Err(Error::decode(missing("delta")(), data));
                };
                self.delta(index()?, kind, delta, data)?;
            }
            Some(EventType::ContentBlockStop) => self.stop(index()?)?,
            Some(EventType::MessageDelta) => return self.message_delta(frame),
            Some(EventType::Error) => return Err(failure(data)),
            Some(EventType::Other) | None => {}
        }
        Ok(Flow::More)
    }
}

#[cfg(test)]
mod growth_tests {
    use super::*;
    use crate::protocol::{Decoder, Frame};
    use crate::sse::DecodeLimits;

    #[test]
    fn hosted_argument_depth_is_rejected_before_buffering() {
        let limits = DecodeLimits::default();
        let mut decoder = MessagesDecoder::new(false);
        let mut out = Vec::new();
        let start = json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "server_tool_use", "id": "id", "name": "tool"}})
        .to_string();
        decoder
            .frame(
                Frame {
                    event: "message",
                    data: &start,
                },
                &mut out,
            )
            .unwrap();
        let delta = json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "[".repeat(limits.depth + 1)}})
        .to_string();
        let result = decoder.frame(
            Frame {
                event: "message",
                data: &delta,
            },
            &mut out,
        );
        assert!(
            result.is_err(),
            "over-depth input was retained in the private buffer"
        );
        assert_eq!(decoder.block(0).unwrap().json, "");
    }

    #[test]
    fn private_hosted_and_tool_json_obey_argument_and_reply_limits() {
        for kind in ["tool_use", "server_tool_use"] {
            for (reply_bytes, argument_bytes) in [(1024, 8), (128, 1024)] {
                let limits = DecodeLimits {
                    reply_bytes,
                    argument_bytes,
                    ..Default::default()
                };
                let mut decoder = MessagesDecoder::new(false);
                decoder.w = Writer::with_limits(KEY, limits);
                let mut out = Vec::new();
                let start = json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": kind, "id": "id", "name": "tool"}})
                .to_string();
                decoder
                    .frame(
                        Frame {
                            event: "message",
                            data: &start,
                        },
                        &mut out,
                    )
                    .unwrap();
                let delta = json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "123456789"}})
                .to_string();
                let error = decoder
                    .frame(
                        Frame {
                            event: "message",
                            data: &delta,
                        },
                        &mut out,
                    )
                    .unwrap_err();
                assert_eq!(error.kind, Kind::Decode);
                assert_eq!(decoder.block(0).unwrap().json, "");
            }
        }
    }
}
