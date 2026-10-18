//! Messages frames into parts: each content block becomes one part, in wire
//! order, whose provider item is the block as the provider stated it
//! complete, and only when the wire would take that block back. Fields are
//! read leniently, so an invented field, or a known one of another type,
//! never fails a reply; a block the decoder cannot build from does.
//!
//! Citations are not modeled: their deltas land on the text block's item,
//! which replays them.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use super::KEY;
use crate::error::{Error, Kind as ErrorKind};
use crate::json::Lenient;
use crate::protocol::Flow;
use crate::spec::{Reason, SmolStr, Usage};
use crate::writer::{AUTO_INDEX, CallFragment, End, Open, Writer, Writes, merge};

/// The events the decoder reads; any other type is skipped. `message` is a
/// whole message, as a gateway may send in place of a stream.
const fn is_known(kind: &str) -> bool {
    matches!(
        kind.as_bytes(),
        b"message"
            | b"message_start"
            | b"content_block_start"
            | b"content_block_delta"
            | b"content_block_stop"
            | b"message_delta"
            | b"message_stop"
            | b"ping"
            | b"error"
    )
}

/// Anthropic's usage counters, read leniently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    /// The part of `cache_creation` written for the one-hour retention.
    cache_creation_long: Option<u64>,
    thinking: Option<u64>,
    /// Requests server tools made (`server_tool_use`).
    server_tools: Option<u64>,
}

impl Counts {
    fn of(usage: Option<&Value>) -> Self {
        let count = |pointer: &str| usage?.at(pointer)?.as_u64_lenient();
        Self {
            input: count("/input_tokens"),
            output: count("/output_tokens"),
            cache_read: count("/cache_read_input_tokens"),
            cache_creation: count("/cache_creation_input_tokens"),
            cache_creation_long: count("/cache_creation/ephemeral_1h_input_tokens"),
            thinking: count("/output_tokens_details/thinking_tokens"),
            server_tools: crate::json::counters(
                usage.and_then(|usage| usage.get("server_tool_use")),
                None,
            ),
        }
    }

    /// The input is `input_tokens` plus the cache reads and writes Anthropic
    /// counts beside it; the output includes thinking.
    fn usage(self) -> Usage {
        Usage {
            input: self.input.map(|uncached| {
                uncached + self.cache_read.unwrap_or(0) + self.cache_creation.unwrap_or(0)
            }),
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_creation,
            cache_write_long: self.cache_creation_long,
            reasoning: self.thinking,
            hosted_tool_calls: self.server_tools.filter(|calls| *calls > 0),
            ..Usage::default()
        }
    }
}

/// How a `stop_reason` ends the turn, the provider's word, and a refusal's
/// explanation.
fn finish_of(reason: &str, details: Option<&Value>) -> (Reason, Option<String>) {
    let normalized = match reason {
        // `pause_turn` is a server-tool loop that stopped at its limit: the
        // turn is replayed as it is to resume it.
        "end_turn" | "stop_sequence" | "pause_turn" => Reason::Stop,
        "max_tokens" | "model_context_window_exceeded" => Reason::Length,
        "tool_use" => Reason::ToolCalls,
        "refusal" => {
            let explanation = details
                .and_then(|details| details.str("explanation"))
                .filter(|explanation| !explanation.is_empty())
                .unwrap_or("The model refused to complete the request");
            return (Reason::ContentFilter, Some(explanation.to_owned()));
        }
        _ => Reason::Other,
    };
    (normalized, None)
}

/// What an open content block is, for the checks its end makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Thinking,
    Redacted,
    Call,
    Opaque,
}

/// `content_block_stop` states a block complete; a stop reason states every
/// block still open complete, but a call whose input is not yet JSON.
pub(crate) struct MessagesDecoder {
    w: Writer,
    /// Each open block's kind and the input JSON streamed to it.
    open: BTreeMap<usize, (Kind, String)>,
    /// Every index a block opened at, closed or not.
    started: BTreeSet<usize>,
    /// Whether a block other than a leading `fallback` marker opened.
    opened: bool,
    unsigned_thinking: bool,
    /// The counters `message_start` reported, for a terminal `message_delta`
    /// that does not repeat them.
    start: Counts,
    /// The provider's usage objects, `message_start`'s with the terminal
    /// delta's fields over it.
    raw_usage: Option<Value>,
    message_id: Option<SmolStr>,
    model: Option<SmolStr>,
    container: Option<Value>,
}

/// The block or delta under `key`, which must name its `type`.
fn item(frame: &Value, key: &str, data: &str) -> Result<Map<String, Value>, Error> {
    match frame.get(key) {
        Some(Value::Object(item)) if item.get("type").is_some_and(Value::is_string) => {
            Ok(item.clone())
        }
        _ => Err(Error::decode(
            format!("an Anthropic event carries no `{key}` with a string `type`"),
            data,
        )),
    }
}

/// The content block the event addresses.
fn index(frame: &Value, data: &str) -> Result<usize, Error> {
    frame
        .u64("index")
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index < AUTO_INDEX)
        .ok_or_else(|| Error::decode("an Anthropic event names no block index", data))
}

/// Whether `json` is input a call may end with: none yet, or an object.
fn whole_input(json: &str) -> bool {
    json.trim().is_empty() || matches!(serde_json::from_str(json), Ok(Value::Object(_)))
}

impl MessagesDecoder {
    /// A fresh decoder for one reply. `unsigned_thinking` is whether the
    /// host takes thinking back without a signature.
    pub(crate) fn new(unsigned_thinking: bool) -> Self {
        Self {
            w: Writer::new(KEY).with_call_id_slot(),
            open: BTreeMap::new(),
            started: BTreeSet::new(),
            opened: false,
            unsigned_thinking,
            start: Counts::default(),
            raw_usage: None,
            message_id: None,
            model: None,
            container: None,
        }
    }

    /// Open the content block at `index` as the provider states it.
    fn start(&mut self, index: usize, block: Map<String, Value>) -> Result<(), Error> {
        self.started.insert(index);
        let block = Value::Object(block);
        let kind = block.str("type").unwrap_or_default();
        // A leading `fallback` names the model that took over; one after
        // output began is a fallback no part can represent (pi's rule).
        if kind == "fallback" {
            if self.opened {
                return Err(Error::new(
                    ErrorKind::Other,
                    "Anthropic performed an unsupported mid-output model fallback",
                ));
            }
            self.open.insert(index, (Kind::Opaque, String::new()));
            return self.w.open(index, Open::Opaque { replay: false }, block);
        }
        self.opened = true;
        let (opened, kind, text) = match kind {
            "text" => (Open::Text, Kind::Text, block.str("text")),
            "thinking" => (
                Open::Reasoning { redacted: false },
                Kind::Thinking,
                block.str("thinking"),
            ),
            "redacted_thinking" => (Open::Reasoning { redacted: true }, Kind::Redacted, None),
            "tool_use" => {
                self.open.insert(index, (Kind::Call, String::new()));
                let id = block.str("id").unwrap_or_default().into();
                let input = block.get("input").cloned().unwrap_or_default();
                match block.str("name").filter(|name| !name.is_empty()) {
                    Some(name) => {
                        let name = name.into();
                        self.w.open(index, Open::Call { id, name }, block)?;
                    }
                    // A nameless call: the writer drops it.
                    None => self.w.fragment(
                        Some(index),
                        CallFragment {
                            id: Some(&id),
                            ..CallFragment::default()
                        },
                    )?,
                }
                // A whole reply states the input on the block; a stream
                // streams it, and the fragments win.
                return self.w.announce(index, input);
            }
            _ => (Open::Opaque { replay: true }, Kind::Opaque, None),
        };
        let text = text.unwrap_or_default().to_owned();
        self.open.insert(index, (kind, String::new()));
        self.w.open(index, opened, block)?;
        self.w.push(index, &text)
    }

    /// Apply a delta to the block at `index`: text and thinking grow both
    /// the part and its item, input JSON is assembled, a citation is
    /// appended to the item, and any other delta merges into the item by
    /// key.
    fn delta(&mut self, index: usize, delta: &Map<String, Value>, data: &str) -> Result<(), Error> {
        let kind = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // The fragment a block is built from must be text.
        let fragment = |key: &str| {
            delta.get(key).and_then(Value::as_str).ok_or_else(|| {
                Error::decode(
                    format!("an Anthropic `{kind}` carries no string `{key}`"),
                    data,
                )
            })
        };
        // A gateway that skips `content_block_start` still streams the
        // block's text, so its first delta opens it.
        if !self.started.contains(&index) {
            let opened = match kind {
                "text_delta" => Some(json!({"type": "text", "text": ""})),
                "thinking_delta" => Some(json!({"type": "thinking", "thinking": ""})),
                _ => None,
            };
            if let Some(Value::Object(block)) = opened {
                self.start(index, block)?;
            }
        }
        match kind {
            "text_delta" => self.w.push(index, fragment("text")?)?,
            "thinking_delta" => self.w.push(index, fragment("thinking")?)?,
            "input_json_delta" => {
                let fragment = fragment("partial_json")?;
                let Some((kind @ (Kind::Call | Kind::Opaque), json)) = self.open.get_mut(&index)
                else {
                    return Err(Error::decode(
                        format!(
                            "Anthropic streamed input to content block {index}, which takes none"
                        ),
                        data,
                    ));
                };
                json.push_str(fragment);
                if *kind == Kind::Call {
                    self.w.push(index, fragment)?;
                }
                return Ok(());
            }
            "citations_delta" => {
                let citation = delta.get("citation").cloned().unwrap_or_default();
                return self.w.edit(index, |item| {
                    if let Some(item) = item.as_object_mut() {
                        match item.get_mut("citations") {
                            Some(Value::Array(citations)) => citations.push(citation),
                            _ => {
                                item.insert("citations".to_owned(), Value::Array(vec![citation]));
                            }
                        }
                    }
                });
            }
            _ => {}
        }
        // Text and signatures concatenate; deltas never seen before land in
        // the item too.
        self.w.edit(index, |item| merge(item, delta))
    }

    /// End the block at `index` as stated complete. Its item becomes the
    /// part's state only when the wire takes it back: text that is not
    /// blank, thinking with its signature (unless the host takes it
    /// unsigned), redacted thinking with its data, and a call with an
    /// object `input`, set from what streamed. A hosted item whose input is
    /// not an object never completed.
    fn stop(&mut self, index: usize) -> Result<(), Error> {
        let Some((kind, json)) = self.open.remove(&index) else {
            return self.w.finish(index);
        };
        if kind == Kind::Call && json.is_empty() {
            // Input stated on the block that is no object stands as the
            // call's argument text, which the writer judges.
            let mut stated = None;
            self.w
                .edit(index, |item| stated = item.get("input").cloned())?;
            if let Some(input) = stated.filter(|input| !input.is_object() && !input.is_null()) {
                self.w.push(index, &input.to_string())?;
            }
        }
        let streamed = (!json.is_empty()).then(|| {
            if json.trim().is_empty() {
                Ok(json!({}))
            } else {
                serde_json::from_str::<Value>(&json)
            }
        });
        let unsigned = self.unsigned_thinking;
        let mut complete = true;
        self.w.edit(index, |item| {
            let input = match streamed {
                Some(Ok(parsed)) => Ok(Some(parsed)),
                Some(Err(_)) => Err(()),
                None => Ok(item.get("input").cloned()),
            };
            let kept = match (kind, input) {
                (Kind::Text, _) => !item.str("text").unwrap_or_default().trim().is_empty(),
                (Kind::Thinking, _) => {
                    unsigned || item.str("signature").is_some_and(|s| !s.is_empty())
                }
                (Kind::Redacted, _) => item.str("data").is_some_and(|data| !data.is_empty()),
                (Kind::Opaque, Ok(None)) => true,
                (Kind::Call | Kind::Opaque, Ok(Some(input @ Value::Object(_)))) => {
                    set_input(item, input);
                    true
                }
                (Kind::Call, Ok(None | Some(Value::Null))) => {
                    set_input(item, json!({}));
                    true
                }
                (Kind::Call, _) => false,
                (Kind::Opaque, _) => {
                    complete = false;
                    true
                }
            };
            if !kept {
                *item = Value::Null;
            }
        })?;
        if complete {
            self.w.finish(index)
        } else {
            self.w.close(index)
        }
    }

    /// Note the metadata a whole message or `message_start` states.
    fn metadata(&mut self, message: &Value) {
        self.start = Counts::of(message.get("usage"));
        self.raw_usage = message
            .get("usage")
            .filter(|usage| usage.is_object())
            .cloned();
        self.message_id = message.str("id").map(Into::into);
        self.model = message.str("model").map(Into::into);
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
        usage: Counts,
        stop_reason: Option<&str>,
        details: Option<&Value>,
    ) -> Result<Flow, Error> {
        let open: Vec<usize> = self
            .open
            .iter()
            .filter(|(_, (kind, json))| *kind != Kind::Call || whole_input(json))
            .map(|(index, _)| *index)
            .collect();
        for index in open {
            self.stop(index)?;
        }
        if let Some(container) = &self.container {
            let index = self.w.fresh_index();
            let item = json!({"type": "container", "container": container});
            self.w
                .whole(index, Open::Opaque { replay: true }, item, "")?;
        }
        let (reason, message) = stop_reason.map_or((None, None), |raw| {
            let (reason, message) = finish_of(raw, details);
            (Some((reason, Some(raw.into()))), message)
        });
        self.w.end(End {
            usage: usage.usage(),
            reason,
            response_id: self.message_id.clone(),
            model: self.model.clone(),
            message,
            raw_usage: self.raw_usage.clone(),
        })
    }

    /// A whole message, written block by block as a stream writes it, then
    /// ended. Empty content is a turn like any other: the stop reason
    /// decides how it ends.
    fn whole(&mut self, message: &Value, data: &str) -> Result<Flow, Error> {
        self.metadata(message);
        for (index, block) in message.arr("content").iter().enumerate() {
            let Some(block) = block
                .as_object()
                .filter(|block| block.get("type").is_some_and(Value::is_string))
            else {
                return Err(Error::decode(
                    format!("Anthropic content block {index} has no string `type`"),
                    data,
                ));
            };
            self.start(index, block.clone())?;
            self.stop(index)?;
        }
        self.end(
            self.start,
            message.str("stop_reason"),
            message.get("stop_details"),
        )
    }

    /// The terminal `message_delta`'s counters, with `message_start`'s for
    /// those it omits. A zero input count is a gateway's placeholder; zero
    /// cache counts are counts.
    fn terminal(&self, usage: Option<&Value>) -> Counts {
        let terminal = Counts::of(usage);
        Counts {
            input: terminal
                .input
                .filter(|tokens| *tokens > 0)
                .or(self.start.input),
            cache_read: terminal.cache_read.or(self.start.cache_read),
            cache_creation: terminal.cache_creation.or(self.start.cache_creation),
            cache_creation_long: terminal
                .cache_creation_long
                .or(self.start.cache_creation_long),
            ..terminal
        }
    }
}

/// `item`'s `input` set to `input`.
fn set_input(item: &mut Value, input: Value) {
    if let Some(fields) = item.as_object_mut() {
        fields.insert("input".to_owned(), input);
    }
}

impl Writes for MessagesDecoder {
    fn writer(&mut self) -> &mut Writer {
        &mut self.w
    }

    fn data(&mut self, data: &str) -> Result<Flow, Error> {
        if data.trim() == "[DONE]" {
            // The provider's end is the `message_delta` with a stop reason.
            return Ok(Flow::More);
        }
        let frame: Value =
            serde_json::from_str(data).map_err(|error| Error::decode(error, data))?;
        let Some(kind) = frame.str("type").filter(|kind| is_known(kind)) else {
            return Ok(Flow::More);
        };
        match kind {
            "message" => return self.whole(&frame, data),
            // A `message_start` without a message (a Bedrock-compatible
            // gateway sends one) says nothing.
            "message_start" => {
                if let Some(message) = frame.get("message").filter(|message| message.is_object()) {
                    self.metadata(message);
                }
            }
            "content_block_start" => {
                let block = item(&frame, "content_block", data)?;
                self.start(index(&frame, data)?, block)?;
            }
            "content_block_delta" => {
                let delta = item(&frame, "delta", data)?;
                self.delta(index(&frame, data)?, &delta, data)?;
            }
            "content_block_stop" => self.stop(index(&frame, data)?)?,
            "message_delta" => {
                let delta = frame.get("delta");
                self.note_container(delta.and_then(|delta| delta.get("container")));
                // Only a `message_delta` with a stop reason ends the reply.
                let Some(reason) = delta.and_then(|delta| delta.str("stop_reason")) else {
                    return Ok(Flow::More);
                };
                let usage = self.terminal(frame.get("usage"));
                if let Some(Value::Object(delta)) = frame.get("usage") {
                    let raw = self.raw_usage.get_or_insert_with(|| json!({}));
                    if let Some(raw) = raw.as_object_mut() {
                        raw.extend(
                            delta
                                .iter()
                                .map(|(key, value)| (key.clone(), value.clone())),
                        );
                    }
                }
                let details = delta.and_then(|delta| delta.get("stop_details"));
                return self.end(usage, Some(reason), details);
            }
            "error" => {
                let error = frame.get("error").unwrap_or(&frame);
                let message = error
                    .str("message")
                    .map_or_else(|| frame.to_string(), str::to_owned);
                return Err(Error::provider(error.str("type"), message));
            }
            _ => {}
        }
        Ok(Flow::More)
    }
}
