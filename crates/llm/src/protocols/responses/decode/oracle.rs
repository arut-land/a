//! The decoder as it read frames before it read each once (a typed read
//! for deltas, a `Value` for any other frame), kept as the reference the
//! decoder must agree with event for event. Responses frames into parts: one part per output item, in the order the
//! items arrive, each holding the item as the provider stated it complete.
//! Every field is read leniently, so a gateway that omits or retypes a field
//! no part or finish needs never fails a reply. Delta events, nearly every
//! frame, are read into fields borrowed from the frame; any other frame, or
//! one those fields do not fit, is read as a `Value`.

use std::borrow::Cow;

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_json::{Value, json};

use super::{ItemType, KEY, PartType, Status};
use crate::error::{Error, Kind};
use crate::json::{Lenient, word};
use crate::protocol::Flow;
use crate::spec::{Reason, SmolStr, Usage};
use crate::writer::{CallFragment, End, Open, Writer, Writes};

/// The events the decoder reads; any other type is skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
enum Event {
    #[serde(rename = "error", alias = "response.failed")]
    Failed,
    #[serde(rename = "response.completed")]
    Completed,
    #[serde(rename = "response.incomplete")]
    Incomplete,
    #[serde(rename = "response.output_item.added")]
    Added,
    #[serde(rename = "response.output_item.done")]
    Done,
    // A refusal is the assistant's message for that turn.
    #[serde(
        rename = "response.output_text.delta",
        alias = "response.refusal.delta"
    )]
    Text,
    #[serde(rename = "response.reasoning_summary_text.delta")]
    Summary,
    #[serde(rename = "response.reasoning_text.delta")]
    Reasoning,
    #[serde(rename = "response.function_call_arguments.delta")]
    #[serde(alias = "response.custom_tool_call_input.delta")]
    Arguments,
}

/// The reasons a response is incomplete that have a finish of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Incomplete {
    MaxOutputTokens,
    ContentFilter,
}

/// A delta event's fields, borrowed from its frame.
#[derive(Deserialize)]
struct Delta<'a> {
    /// Only a delta event reads as one, so any other frame stops at its
    /// type.
    #[serde(rename = "type", deserialize_with = "delta_event")]
    event: Event,
    #[serde(rename = "delta", borrow)]
    text: Cow<'a, str>,
    output_index: Option<u64>,
    item_id: Option<&'a str>,
    content_index: Option<u64>,
    summary_index: Option<u64>,
    /// A frame stating an item or a response is no delta this reads: the
    /// `Value` path addresses an event by its item's id.
    #[serde(rename = "item", default, deserialize_with = "refuse")]
    _item: (),
    #[serde(rename = "response", default, deserialize_with = "refuse")]
    _response: (),
}

fn delta_event<'de, D: Deserializer<'de>>(kind: D) -> Result<Event, D::Error> {
    match Event::deserialize(kind)? {
        event @ (Event::Text | Event::Summary | Event::Reasoning | Event::Arguments) => Ok(event),
        _ => Err(de::Error::custom("not a delta event")),
    }
}

fn refuse<'de, D: Deserializer<'de>>(_: D) -> Result<(), D::Error> {
    Err(de::Error::custom("not a delta event"))
}

impl<'a> Delta<'a> {
    fn lenient(event: Event, frame: &'a Value) -> Self {
        Self {
            event,
            text: frame.str("delta").unwrap_or_default().into(),
            output_index: frame.u64("output_index"),
            item_id: address(frame),
            content_index: frame.u64("content_index"),
            summary_index: frame.u64("summary_index"),
            _item: (),
            _response: (),
        }
    }
}

/// What an output item becomes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Item {
    #[default]
    Message,
    Reasoning,
    Call,
    Opaque,
}

impl Item {
    fn of(item: &Value) -> Self {
        match ItemType::of(item.get("type")) {
            Some(ItemType::Message) => Self::Message,
            Some(ItemType::Reasoning) => Self::Reasoning,
            Some(ItemType::FunctionCall | ItemType::CustomToolCall) => Self::Call,
            _ => Self::Opaque,
        }
    }
}

/// One output item as it streams.
#[derive(Default)]
struct Slot {
    /// Its wire index in the writer, and the output index it holds until
    /// another item takes it.
    at: usize,
    index: Option<usize>,
    kind: Item,
    custom: bool,
    id: Option<String>,
    open: bool,
    /// The event its deltas came in, and the content or summary part that
    /// last extended it.
    field: Option<Event>,
    part: u64,
    /// Whether the call's id was stated when it opened.
    named: bool,
}

impl Slot {
    /// End the call with what its done `item` states, or else with what
    /// streamed (a custom call's input closed as the object it opened);
    /// argument JSON that does not extend what the writer has restates it.
    fn end_call(&self, w: &mut Writer, item: Option<&Value>) -> Result<(), Error> {
        let held = w.text(self.at).unwrap_or_default().to_owned();
        let whole =
            item.and_then(arguments_of)
                .unwrap_or_else(|| match (self.custom, held.as_str()) {
                    (false, _) => held.clone(),
                    (true, "") => r#"{"input":""}"#.to_owned(),
                    (true, held) => format!("{held}\"}}"),
                });
        let rest = whole.strip_prefix(&held);
        if rest.is_none() {
            w.restate(self.at, &whole)?;
        }
        let stated = |key: &str| item.and_then(|item| item.str(key));
        let (id, name) = (stated("call_id").filter(|_| !self.named), stated("name"));
        let arguments = Some(rest.unwrap_or_default());
        let fragment = CallFragment {
            id,
            name,
            arguments,
        };
        w.fragment(self.at, fragment)
    }
}

fn item_id(item: &Value) -> Option<&str> {
    item.str("id").filter(|id| !id.is_empty())
}

/// The item id an event addresses.
fn address(frame: &Value) -> Option<&str> {
    let id = frame.at("/item/id").and_then(Value::as_str);
    id.or_else(|| frame.str("item_id"))
}

fn ciphertext(item: &Value) -> Option<&Value> {
    let cipher = item.get("encrypted_content");
    cipher.filter(|cipher| cipher.as_str().is_some_and(|text| !text.is_empty()))
}

fn arguments_of(item: &Value) -> Option<String> {
    if ItemType::of(item.get("type")) == Some(ItemType::CustomToolCall) {
        let input = item.str("input")?;
        return Some(json!({ "input": input }).to_string());
    }
    match item.get("arguments")? {
        Value::String(arguments) if arguments.is_empty() => None,
        Value::String(arguments) => Some(arguments.clone()),
        Value::Null => None,
        arguments => Some(arguments.to_string()),
    }
}

fn text_of(item: &Value) -> String {
    let texts = |key: &str| -> Vec<&str> {
        if let Some(Value::String(text)) = item.get(key) {
            return vec![text];
        }
        let text = |part| [Value::as_str(part), part.str("text"), part.str("refusal")];
        let parts = item.arr(key).iter();
        parts
            .filter_map(|part| text(part).into_iter().flatten().next())
            .collect()
    };
    match Item::of(item) {
        Item::Message => texts("content").concat(),
        Item::Reasoning => Some(texts("summary"))
            .filter(|summary| !summary.is_empty())
            .unwrap_or_else(|| texts("content"))
            .join("\n\n"),
        Item::Call | Item::Opaque => String::new(),
    }
}

/// How the response ended: its reason, the provider's word, and why a reply
/// that did not complete stopped.
fn finish_of(response: &Value) -> (Reason, Option<SmolStr>, Option<String>) {
    let Some(status) = response.str("status") else {
        return (Reason::Stop, None, None);
    };
    let reason = response
        .at("/incomplete_details/reason")
        .and_then(Value::as_str);
    let reason = reason.filter(|reason| !reason.is_empty());
    let message = match (word(status), reason) {
        (Some(Status::Completed), _) => return (Reason::Stop, None, None),
        (Some(Status::Incomplete), Some(reason)) => match word(reason) {
            Some(Incomplete::MaxOutputTokens) => {
                return (Reason::Length, Some(reason.into()), None);
            }
            Some(Incomplete::ContentFilter) => {
                return (Reason::ContentFilter, Some(reason.into()), None);
            }
            None => format!("Response incomplete: {reason}"),
        },
        (Some(Status::Incomplete), None) => "Response incomplete without a provider reason".into(),
        (Some(Status::Failed | Status::Cancelled), _) => {
            let stated: Vec<&str> = ["/error/code", "/error/message"]
                .iter()
                .filter_map(|pointer| response.at(pointer)?.as_str())
                .collect();
            if stated.is_empty() {
                format!("Response {status}")
            } else {
                stated.join(": ")
            }
        }
        (Some(Status::Queued | Status::InProgress), _) => format!("Response ended while {status}"),
        (None, _) => format!("Response ended with the unknown status `{status}`"),
    };
    // An incomplete reply's word names why.
    let raw = match reason.filter(|_| status == "incomplete") {
        Some(reason) => smol_str::format_smolstr!("incomplete: {reason}"),
        None => status.into(),
    };
    (Reason::Other, Some(raw), Some(message))
}

fn usage_of(usage: &Value) -> Usage {
    let count = |pointer: &str| usage.at(pointer).and_then(Lenient::as_u64_lenient);
    Usage {
        input: count("/input_tokens"),
        output: count("/output_tokens"),
        cache_read: count("/input_tokens_details/cached_tokens"),
        cache_write: count("/input_tokens_details/cache_write_tokens"),
        reasoning: count("/output_tokens_details/reasoning_tokens"),
        ..Usage::default()
    }
}

/// The error a failure frame or a failed response reports.
fn failure(frame: &Value) -> Error {
    let error = frame.get("error").filter(|error| error.is_object());
    let error = error
        .or_else(|| frame.at("/response/error"))
        .unwrap_or(frame);
    let code = error.str("code").or_else(|| error.str("type"));
    let message = error.str("message").map(str::to_owned);
    Error::provider(code, message.unwrap_or_else(|| frame.to_string()))
}

fn output_index(index: Option<u64>) -> Result<Option<usize>, Error> {
    let fits = |index: u64| {
        let message = || format!("output_index {index} is out of range");
        usize::try_from(index).map_err(|_| Error::new(Kind::Decode, message()))
    };
    index.map(fits).transpose()
}

pub(crate) struct Oracle {
    w: Writer,
    /// Every output item, in the order it arrived.
    slots: Vec<Slot>,
    /// Reasoning parts waiting for the item they precede to complete.
    held: Vec<usize>,
    /// A custom call's input as the JSON string it streams in.
    escaped: Vec<u8>,
}

impl Oracle {
    pub(crate) fn new() -> Self {
        let mut w = Writer::new(KEY).with_call_id_slot();
        // A terminal may state items the stream never announced; they take
        // their place by output index.
        w.order_by_index();
        Self {
            w,
            slots: Vec::new(),
            held: Vec::new(),
            escaped: Vec::new(),
        }
    }

    fn holding(&self, index: usize) -> Option<usize> {
        self.slots.iter().position(|slot| slot.index == Some(index))
    }

    /// The slot an event addresses: by output index, then by item id, then
    /// the latest item while the stream is writing it as one of `kind`.
    fn addressed(&self, index: Option<usize>, id: Option<&str>, kind: Item) -> Option<usize> {
        let by_id = || {
            let id = id.filter(|id| !id.is_empty())?;
            (self.slots.iter()).rposition(|slot| slot.id.as_deref() == Some(id))
        };
        let latest = || {
            let at = self.slots.len().checked_sub(1)?;
            let slot = &self.slots[at];
            let writing = slot.open && slot.kind == kind;
            (writing && (index.is_none() || slot.index.is_none())).then_some(at)
        };
        let held = index.and_then(|index| self.holding(index));
        held.or_else(by_id).or_else(latest)
    }

    fn added(&mut self, index: Option<usize>, item: &Value) -> Result<usize, Error> {
        if let Some(previous) = index.and_then(|index| self.holding(index)) {
            self.slots[previous].index = None;
            self.vacate(previous)?;
        }
        let at = match index {
            Some(index) if !self.slots.iter().any(|slot| slot.at == index) => index,
            _ => self.w.fresh_index(),
        };
        let (kind, ty) = (Item::of(item), ItemType::of(item.get("type")));
        let (id, name) = (item.str("call_id"), item.str("name"));
        let (open, state) = match kind {
            Item::Message => (Open::Text, Value::Null),
            Item::Reasoning => (Open::Reasoning { redacted: false }, Value::Null),
            Item::Call => {
                let (id, name) = (id.unwrap_or_default(), name.unwrap_or_default());
                let open = Open::Call {
                    id: id.into(),
                    name: name.into(),
                };
                (open, Value::Null)
            }
            // An item that names no type is kept and never sent back.
            Item::Opaque => {
                let replay = ty.is_some_and(ItemType::replays);
                (Open::Opaque { replay }, item.clone())
            }
        };
        self.w.open(at, open, state)?;
        let custom = ty == Some(ItemType::CustomToolCall);
        self.slots.push(Slot {
            at,
            index,
            kind,
            custom,
            id: item_id(item).map(str::to_owned),
            open: true,
            named: id.is_some_and(|id| !id.is_empty()),
            ..Slot::default()
        });
        let slot = self.slots.len() - 1;
        if kind == Item::Call {
            let arguments = item.str(if custom { "input" } else { "arguments" });
            self.stream(slot, arguments.unwrap_or_default().into())?;
        }
        Ok(slot)
    }

    /// Write `fragment` on to the call at `slot`: a custom call's input as
    /// the `{"input": …}` string it extends.
    fn stream(&mut self, slot: usize, fragment: Cow<'_, str>) -> Result<(), Error> {
        let call = &self.slots[slot];
        if !call.custom || fragment.is_empty() {
            return self.w.push(call.at, fragment);
        }
        let first = self.w.text(call.at).is_none_or(str::is_empty);
        self.escaped.clear();
        if first {
            self.escaped.extend_from_slice(b"{\"input\":");
        }
        let escaped = serde_json::to_writer(&mut self.escaped, &fragment);
        escaped.map_err(|error| Error::decode(error, &fragment))?;
        // The string stays open: later input extends it.
        self.escaped.pop();
        let json = std::str::from_utf8(&self.escaped).unwrap_or_default();
        self.w.push(call.at, if first { json } else { &json[1..] })
    }

    /// Close the open `slot` without the item that would complete it:
    /// another item took its output index.
    fn vacate(&mut self, slot: usize) -> Result<(), Error> {
        let slot = &mut self.slots[slot];
        if !slot.open {
            return Ok(());
        }
        slot.open = false;
        if slot.kind == Item::Call {
            slot.end_call(&mut self.w, None)?;
        }
        self.w.close(slot.at)?;
        self.release(false)
    }

    /// Settle the held reasoning: complete when the item after it is.
    fn release(&mut self, complete: bool) -> Result<(), Error> {
        for at in std::mem::take(&mut self.held) {
            if complete {
                self.w.finish(at)?;
            } else {
                self.w.close(at)?;
            }
        }
        Ok(())
    }

    fn delta(&mut self, delta: Delta<'_>) -> Result<(), Error> {
        let (event, text) = (delta.event, delta.text);
        if text.is_empty() {
            return Ok(());
        }
        let index = output_index(delta.output_index)?;
        let id = delta.item_id;
        let (kind, part) = match event {
            Event::Text => (Item::Message, delta.content_index),
            Event::Summary => (Item::Reasoning, delta.summary_index),
            Event::Reasoning => (Item::Reasoning, delta.content_index),
            _ => {
                let call = self.addressed(index, id, Item::Call);
                let open = |at: &usize| self.slots[*at].open && self.slots[*at].kind == Item::Call;
                return (call.filter(open)).map_or(Ok(()), |call| self.stream(call, text));
            }
        };
        let slot = match self.addressed(index, id, kind) {
            // A delta for an item that already closed is left to the item,
            // which states its text.
            Some(at) if self.slots[at].kind == kind && !self.slots[at].open => return Ok(()),
            Some(at) if self.slots[at].kind == kind => at,
            _ if kind == Item::Message => self.added(index, &json!({"type": ItemType::Message}))?,
            _ => self.added(index, &json!({"type": ItemType::Reasoning}))?,
        };
        let streamed = &mut self.slots[slot];
        if streamed.field.is_some_and(|field| field != event) {
            return Ok(());
        }
        let part = part.unwrap_or(0);
        // Summary parts of one reasoning item are paragraphs.
        let wrote = self
            .w
            .text(streamed.at)
            .is_some_and(|text| !text.is_empty());
        if streamed.part != part && kind == Item::Reasoning && wrote {
            self.w.push(streamed.at, "\n\n")?;
        }
        (streamed.field, streamed.part) = (Some(event), part);
        self.w.push(streamed.at, text)
    }

    /// The done event for `item`, at output `index`, addressing item `id`.
    fn item_done(
        &mut self,
        index: Option<usize>,
        id: Option<&str>,
        item: &Value,
    ) -> Result<(), Error> {
        let (kind, stated) = (Item::of(item), item_id(item));
        let known = self.addressed(index, id, kind);
        let slot = match known.map(|at| (at, &self.slots[at])) {
            // At a named output index the item is the one there, whatever
            // id it states.
            Some((at, slot))
                if slot.open
                    && slot.kind == kind
                    && (index.is_some()
                        || stated
                            .zip(slot.id.as_deref())
                            .is_none_or(|(id, known)| id == known)) =>
            {
                at
            }
            // The item this one restates is already done.
            Some((_, slot)) if stated.is_some() && slot.id.as_deref() == stated => return Ok(()),
            _ => self.added(index, item)?,
        };
        self.done(slot, item.clone())
    }

    fn done(&mut self, slot: usize, mut item: Value) -> Result<(), Error> {
        let done = &mut self.slots[slot];
        done.open = false;
        if done.id.is_none() {
            done.id = item_id(&item).map(str::to_owned);
        }
        let (at, kind) = (done.at, done.kind);
        // A call with no name is dropped, so the reasoning it held has no
        // item to go with.
        let unnamed = item.str("name").is_none_or(str::is_empty);
        let untitled = self.w.call(at).is_none_or(|call| call.name.is_none());
        let nameless = kind == Item::Call && untitled && unnamed;
        let text = text_of(&item);
        let streamed = self.w.text(at).unwrap_or_default();
        match kind {
            Item::Call => done.end_call(&mut self.w, Some(&item))?,
            Item::Opaque => {}
            _ if !text.is_empty() => match text.strip_prefix(streamed) {
                Some(rest) => self.w.push(at, rest)?,
                // The done item states the whole text.
                None => self.w.restate(at, &text)?,
            },
            // An item that leaves its text out states it, so it replays as
            // the part it is. Raw reasoning text stays out: as a summary it
            // would replay one the provider never produced.
            _ if streamed.is_empty() || done.field == Some(Event::Reasoning) => {}
            Item::Message => {
                let part =
                    json!({"type": PartType::OutputText, "text": streamed, "annotations": []});
                item["content"] = json!([part]);
            }
            Item::Reasoning => {
                item["summary"] = json!([{"type": PartType::SummaryText, "text": streamed}]);
            }
        }
        let reasoning = kind == Item::Reasoning;
        self.w.edit(at, |stored| *stored = item)?;
        if reasoning {
            // Reasoning is sent only with the item it precedes, so it is
            // complete only once that item is.
            self.held.push(at);
            return Ok(());
        }
        self.w.finish(at)?;
        self.release(!nameless)
    }

    /// The slot an item of the terminal's `output` restates: a free one of
    /// its kind with its id, else the one at its output index, else one no
    /// id tells apart that holds no index, else one holding an index whose
    /// output item is of another kind.
    fn restated(
        &self,
        index: usize,
        item: &Value,
        output: &[Value],
        taken: &[usize],
    ) -> Option<usize> {
        let (id, kind) = (item_id(item), Item::of(item));
        let free = |at: &usize| !taken.contains(at) && self.slots[*at].kind == kind;
        let find = |fits: &dyn Fn(&Slot) -> bool| {
            (0..self.slots.len()).find(|at| free(at) && fits(&self.slots[*at]))
        };
        let anonymous = |slot: &Slot| id.is_none() || slot.id.is_none();
        let moved = |slot: &Slot| {
            let streamed = slot.index.map(|streamed| output.get(streamed));
            streamed.is_some_and(|other| other.is_none_or(|other| Item::of(other) != slot.kind))
        };
        id.and_then(|id| find(&|slot| slot.id.as_deref() == Some(id)))
            .or_else(|| self.holding(index).filter(free))
            .or_else(|| find(&|slot| anonymous(slot) && slot.index.is_none()))
            .or_else(|| find(&|slot| anonymous(slot) && moved(slot)))
    }

    fn finish(&mut self, response: &Value) -> Result<Flow, Error> {
        let output = response.arr("output");
        if self.slots.is_empty()
            && !output.iter().any(|item| Item::of(item) == Item::Reasoning)
            && let Some(reasoning) = response.str("reasoning").filter(|text| !text.is_empty())
        {
            // Index 0 orders it before the output; it closes at once, so the
            // output's first item may open there after it.
            let open = Open::Reasoning { redacted: false };
            self.w.whole(0, open, Value::Null, reasoning)?;
        }
        let mut taken = Vec::new();
        for (index, item) in output.iter().enumerate() {
            if !item.is_object() {
                continue;
            }
            let slot = match self.restated(index, item, output, &taken) {
                Some(slot) if self.slots[slot].open => slot,
                Some(slot) => {
                    taken.push(slot);
                    let Slot { at, kind, .. } = self.slots[slot];
                    // Reasoning done without its ciphertext takes the
                    // terminal's.
                    if let (Item::Reasoning, Some(cipher)) = (kind, ciphertext(item)) {
                        self.w.edit(at, |stored| {
                            if ciphertext(stored).is_none()
                                && let Some(fields) = stored.as_object_mut()
                            {
                                fields.insert("encrypted_content".to_owned(), cipher.clone());
                            }
                        })?;
                    }
                    continue;
                }
                None => {
                    let index = Some(index).filter(|index| self.holding(*index).is_none());
                    self.added(index, item)?
                }
            };
            taken.push(slot);
            self.done(slot, item.clone())?;
        }
        // Reasoning still waiting is complete when nothing after it is left
        // open.
        self.release(!self.slots.iter().any(|slot| slot.open))?;
        // A call never done shows what streamed, in a turn that fails.
        for slot in &self.slots {
            if slot.open && slot.kind == Item::Call {
                slot.end_call(&mut self.w, None)?;
            }
        }
        let (reason, raw, message) = finish_of(response);
        // Hosted tools report their requests beside the usage.
        let hosted = crate::json::counters(response.get("tool_usage"), Some("num_requests"));
        let usage = Usage {
            hosted_tool_calls: hosted.filter(|calls| *calls > 0),
            ..response.get("usage").map_or_default(usage_of)
        };
        self.w.end(End {
            usage,
            reason: Some((reason, raw)),
            response_id: item_id(response).map(Into::into),
            model: response.str("model").map(Into::into),
            message,
            raw_usage: response.get("usage").cloned(),
        })
    }

    /// A frame read as a `Value`: every event, leniently.
    pub(super) fn lenient(&mut self, data: &str) -> Result<Flow, Error> {
        if data.trim() == "[DONE]" {
            // The provider's end is `response.completed`.
            return Ok(Flow::More);
        }
        let mut frame: Value =
            serde_json::from_str(data).map_err(|error| Error::decode(error, data))?;
        if !frame.is_object() {
            return Ok(Flow::More);
        }
        let Some(kind) = frame.str("type") else {
            // An untagged body: the whole response, or its error.
            let stated = |key: &str| frame.get(key).is_some();
            let failed = frame.get("error").is_some_and(|error| !error.is_null());
            if failed && !stated("output") && !stated("status") {
                return Err(failure(&frame));
            }
            if ["object", "output", "status", "id"].into_iter().any(stated) {
                return self.finish(&frame);
            }
            return Ok(Flow::More);
        };
        let item = frame.get("item").filter(|item| item.is_object());
        let index = || output_index(frame.u64("output_index"));
        match (word(kind), item) {
            (Some(Event::Failed), _) => return Err(failure(&frame)),
            // `response.incomplete` is a terminal that keeps the partial
            // output and usage, incomplete whatever its `status` says.
            (Some(event @ (Event::Completed | Event::Incomplete)), _) => {
                let response = frame
                    .get_mut("response")
                    .filter(|response| response.is_object());
                let mut response = response.map_or_else(|| json!({}), Value::take);
                if event == Event::Incomplete {
                    response["status"] = json!(Status::Incomplete);
                }
                return self.finish(&response);
            }
            (Some(Event::Added), Some(item)) => self.added(index()?, item).map(drop)?,
            (Some(Event::Done), Some(item)) => self.item_done(index()?, address(&frame), item)?,
            (Some(Event::Added | Event::Done) | None, _) => {}
            (Some(event), _) => self.delta(Delta::lenient(event, &frame))?,
        }
        Ok(Flow::More)
    }
}

impl Writes for Oracle {
    fn writer(&mut self) -> &mut Writer {
        &mut self.w
    }

    fn data(&mut self, data: &str) -> Result<Flow, Error> {
        match typed(data) {
            Some(delta) => self.delta(delta).map(|()| Flow::More),
            None => self.lenient(data),
        }
    }
}

/// A delta frame read into fields borrowed from it; `None` for any other
/// frame, or one those fields do not fit.
fn typed(data: &str) -> Option<Delta<'_>> {
    serde_json::from_str(data).ok()
}
