//! Responses frames into parts: one part per output item, in the order the
//! items arrive, each holding the item as the provider stated it complete.
//! Every field is read leniently, so a gateway that omits or retypes a field
//! no part or finish needs never fails a reply.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::KEY;
use crate::error::{Error, Kind};
use crate::json::Lenient;
use crate::protocol::{Decoder, Flow, Frame};
use crate::spec::{Event, Reason, Usage};
use crate::writer::{CallFragment, End, Open, Writer};

const fn is_lifecycle(kind: &str) -> bool {
    matches!(
        kind.as_bytes(),
        b"response.created"
            | b"response.queued"
            | b"response.in_progress"
            | b"response.completed"
            | b"response.failed"
            | b"response.incomplete"
    )
}

/// The events the decoder reads; any other type is skipped.
fn is_known(kind: &str) -> bool {
    is_lifecycle(kind)
        || matches!(
            kind,
            "error"
                | "response.output_item.added"
                | "response.output_item.done"
                | "response.content_part.added"
                | "response.content_part.done"
                | "response.output_text.delta"
                | "response.output_text.done"
                | "response.refusal.delta"
                | "response.refusal.done"
                | "response.function_call_arguments.delta"
                | "response.function_call_arguments.done"
                | "response.custom_tool_call_input.delta"
                | "response.custom_tool_call_input.done"
                | "response.reasoning_summary_part.added"
                | "response.reasoning_summary_part.done"
                | "response.reasoning_summary_text.delta"
                | "response.reasoning_summary_text.done"
                | "response.reasoning_text.delta"
                | "response.reasoning_text.done"
        )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Item {
    Message,
    Reasoning,
    Call,
    Opaque,
}

impl Item {
    fn of(item: &Value) -> Self {
        match item.str("type") {
            Some("message") => Self::Message,
            Some("reasoning") => Self::Reasoning,
            Some("function_call" | "custom_tool_call") => Self::Call,
            _ => Self::Opaque,
        }
    }
}

/// One output item as it streams.
#[allow(clippy::struct_excessive_bools)]
struct Slot {
    /// Its wire index in the writer.
    at: usize,
    kind: Item,
    id: Option<String>,
    open: bool,
    text: String,
    /// The field its text streams from: `message`, `summary` or `reasoning`.
    field: Option<&'static str>,
    /// The content or summary part the text last came from.
    part: u64,
    arguments: String,
    /// The argument text the writer has.
    sent: String,
    custom: bool,
    named: bool,
    titled: bool,
}

/// Items a client executes: kept, never sent back.
const CLIENT_EXECUTED: &[&str] = &[
    "computer_call",
    "local_shell_call",
    "shell_call",
    "apply_patch_call",
    "mcp_approval_request",
];

fn item_id(item: &Value) -> Option<&str> {
    item.str("id").filter(|id| !id.is_empty())
}

fn arguments_of(item: &Value) -> Option<String> {
    if item.str("type") == Some("custom_tool_call") {
        return item
            .str("input")
            .map(|input| json!({ "input": input }).to_string());
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
        match item.get(key) {
            Some(Value::String(text)) => vec![text.as_str()],
            _ => item
                .arr(key)
                .iter()
                .filter_map(|part| {
                    part.as_str()
                        .or_else(|| part.str("text"))
                        .or_else(|| part.str("refusal"))
                })
                .collect(),
        }
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

/// `item` stating `text` as its only content or summary part.
fn stating(mut item: Value, kind: Item, text: &str) -> Value {
    let (key, part) = match kind {
        Item::Message => (
            "content",
            json!({"type": "output_text", "text": text, "annotations": []}),
        ),
        _ => ("summary", json!({"type": "summary_text", "text": text})),
    };
    if let Some(fields) = item.as_object_mut() {
        fields.insert(key.to_owned(), json!([part]));
    }
    item
}

fn provider_message(response: &Value, fallback: &str) -> String {
    let parts: Vec<&str> = ["/error/code", "/error/message"]
        .iter()
        .filter_map(|pointer| response.at(pointer).and_then(Value::as_str))
        .collect();
    if parts.is_empty() {
        fallback.to_owned()
    } else {
        parts.join(": ")
    }
}

/// How the response ended: its reason, the provider's word, and why a reply
/// that did not complete stopped.
pub(crate) fn finish_of(response: &Value) -> (Reason, Option<String>, Option<String>) {
    let reason = response
        .at("/incomplete_details/reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty());
    match response.str("status") {
        None | Some("completed") => (Reason::Stop, None, None),
        Some("incomplete") => match reason {
            Some("max_output_tokens") => (Reason::Length, Some("max_output_tokens".into()), None),
            Some("content_filter") => (Reason::ContentFilter, Some("content_filter".into()), None),
            Some(reason) => (
                Reason::Other,
                Some(format!("incomplete: {reason}")),
                Some(format!("Response incomplete: {reason}")),
            ),
            None => (
                Reason::Other,
                Some("incomplete".into()),
                Some("Response incomplete without a provider reason".into()),
            ),
        },
        Some(status @ ("failed" | "cancelled")) => (
            Reason::Other,
            Some(status.to_owned()),
            Some(provider_message(response, &format!("Response {status}"))),
        ),
        Some(status @ ("queued" | "in_progress")) => (
            Reason::Other,
            Some(status.to_owned()),
            Some(format!("Response ended while {status}")),
        ),
        Some(status) => (
            Reason::Other,
            Some(status.to_owned()),
            Some(format!("Response ended with the unknown status `{status}`")),
        ),
    }
}

pub(crate) fn usage_of(usage: &Value) -> Usage {
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
pub(crate) fn failure(frame: &Value) -> Error {
    let error = frame
        .get("error")
        .filter(|error| error.is_object())
        .or_else(|| frame.at("/response/error"))
        .unwrap_or(frame);
    let code = error.str("code").or_else(|| error.str("type"));
    let message = error
        .str("message")
        .map_or_else(|| frame.to_string(), str::to_owned);
    Error::provider(code, message)
}

pub(crate) struct ResponsesDecoder {
    w: Writer,
    slots: Vec<Slot>,
    /// The slot at each output index.
    indexed: HashMap<usize, usize>,
    current: Option<usize>,
    /// Reasoning parts waiting for the item they precede to complete.
    held: Vec<usize>,
}

impl ResponsesDecoder {
    pub(crate) fn new() -> Self {
        let mut w = Writer::new(KEY).with_call_id_slot();
        // A terminal may state items the stream never announced; they take
        // their place by output index.
        w.order_by_index();
        Self {
            w,
            slots: Vec::new(),
            indexed: HashMap::new(),
            current: None,
            held: Vec::new(),
        }
    }

    /// The slot `frame` addresses: by output index, then by item id, then
    /// the open slot of `kind` the stream is writing.
    fn addressed(&self, frame: &Value, kind: Item) -> Result<Option<usize>, Error> {
        let index = output_index(frame)?;
        if let Some(slot) = index.and_then(|index| self.indexed.get(&index)) {
            return Ok(Some(*slot));
        }
        let id = frame
            .at("/item/id")
            .and_then(Value::as_str)
            .or_else(|| frame.str("item_id"));
        if let Some(slot) = id.filter(|id| !id.is_empty()).and_then(|id| {
            self.slots
                .iter()
                .rposition(|slot| slot.id.as_deref() == Some(id))
        }) {
            return Ok(Some(slot));
        }
        Ok(self.current.filter(|current| {
            (index.is_none() || !self.indexed.values().any(|slot| slot == current))
                && self
                    .slots
                    .get(*current)
                    .is_some_and(|slot| slot.open && slot.kind == kind)
        }))
    }

    fn added(&mut self, index: Option<usize>, item: &Value) -> Result<usize, Error> {
        if let Some(previous) = index.and_then(|index| self.indexed.get(&index).copied()) {
            self.vacate(previous)?;
        }
        let at = match index {
            Some(index) if !self.slots.iter().any(|slot| slot.at == index) => index,
            _ => self.w.fresh_index(),
        };
        let kind = Item::of(item);
        let custom = item.str("type") == Some("custom_tool_call");
        match kind {
            Item::Message => self.w.open(at, Open::Text, Value::Null)?,
            Item::Reasoning => self
                .w
                .open(at, Open::Reasoning { redacted: false }, Value::Null)?,
            Item::Call => self.w.fragment(
                Some(at),
                CallFragment {
                    id: item.str("call_id"),
                    name: item.str("name"),
                    arguments: None,
                },
            )?,
            // An item that names no type is kept and never sent back.
            Item::Opaque => {
                let replay = item
                    .str("type")
                    .is_some_and(|kind| !CLIENT_EXECUTED.contains(&kind));
                self.w.open(at, Open::Opaque { replay }, item.clone())?;
            }
        }
        let arguments = item.str(if custom { "input" } else { "arguments" });
        self.slots.push(Slot {
            at,
            kind,
            id: item_id(item).map(str::to_owned),
            open: true,
            text: String::new(),
            field: None,
            part: 0,
            arguments: arguments.unwrap_or_default().to_owned(),
            sent: String::new(),
            custom,
            named: item.str("call_id").is_some_and(|id| !id.is_empty()),
            titled: item.str("name").is_some_and(|name| !name.is_empty()),
        });
        let slot = self.slots.len() - 1;
        if let Some(index) = index {
            self.indexed.insert(index, slot);
        }
        self.current = Some(slot);
        if self.slots[slot].kind == Item::Call {
            send(&mut self.slots[slot], &mut self.w)?;
        }
        Ok(slot)
    }

    /// Close the open slot `slot` without the item that would complete it:
    /// another item took its output index.
    fn vacate(&mut self, slot: usize) -> Result<(), Error> {
        let Some(open) = self.slots.get_mut(slot).filter(|slot| slot.open) else {
            return Ok(());
        };
        open.open = false;
        let at = open.at;
        if open.kind == Item::Call {
            flush(open, &mut self.w)?;
        }
        self.w.close(at)?;
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

    fn text(&mut self, frame: &Value, field: &'static str, part: &str) -> Result<(), Error> {
        let delta = frame.str("delta").unwrap_or_default();
        if delta.is_empty() {
            return Ok(());
        }
        let part = frame.u64(part).unwrap_or(0);
        let kind = match field {
            "message" => Item::Message,
            _ => Item::Reasoning,
        };
        let known = self.addressed(frame, kind)?.and_then(|at| {
            let slot = self.slots.get(at)?;
            Some((at, slot.open, slot.kind == kind))
        });
        let slot = match known {
            Some((slot, true, true)) => slot,
            // A delta for an item that already closed is left to the item,
            // which states its text.
            Some((_, false, true)) => return Ok(()),
            _ => self.added(
                output_index(frame)?,
                &json!({"type": if kind == Item::Message { "message" } else { "reasoning" }}),
            )?,
        };
        let Some(streamed) = self.slots.get_mut(slot).filter(|slot| slot.open) else {
            return Ok(());
        };
        if streamed.field.is_some_and(|known| known != field) {
            return Ok(());
        }
        // Summary parts of one reasoning item are paragraphs.
        if streamed.part != part && kind != Item::Message && !streamed.text.is_empty() {
            streamed.text.push_str("\n\n");
            self.w.push(streamed.at, "\n\n")?;
        }
        streamed.field = Some(field);
        streamed.part = part;
        streamed.text.push_str(delta);
        self.w.push(streamed.at, delta)
    }

    fn arguments(&mut self, frame: &Value) -> Result<(), Error> {
        let slot = self.addressed(frame, Item::Call)?;
        if let Some(call) = slot
            .and_then(|slot| self.slots.get_mut(slot))
            .filter(|slot| slot.open && slot.kind == Item::Call)
        {
            call.arguments
                .push_str(frame.str("delta").unwrap_or_default());
            send(call, &mut self.w)?;
        }
        Ok(())
    }

    fn done(&mut self, slot: usize, item: Value) -> Result<(), Error> {
        let Some(done) = self.slots.get_mut(slot) else {
            return Ok(());
        };
        done.open = false;
        if done.id.is_none() {
            done.id = item_id(&item).map(str::to_owned);
        }
        let at = done.at;
        let mut item = item;
        let mut nameless = false;
        match done.kind {
            Item::Call => {
                nameless = !done.titled && item.str("name").is_none_or(str::is_empty);
                let arguments = arguments_of(&item).unwrap_or_else(|| streamed_arguments(done));
                let rest = remainder(done, &arguments, &mut self.w)?;
                self.w.fragment(
                    Some(at),
                    CallFragment {
                        id: item.str("call_id").filter(|_| !done.named),
                        name: item.str("name"),
                        arguments: Some(&rest),
                    },
                )?;
            }
            Item::Message | Item::Reasoning => {
                let text = text_of(&item);
                if text.is_empty() {
                    // Raw reasoning text the done item leaves out stays out:
                    // written in as a summary, it would replay one the
                    // provider never produced.
                    if !done.text.is_empty() && done.field != Some("reasoning") {
                        item = stating(item, done.kind, &done.text);
                    }
                } else if let Some(rest) = text.strip_prefix(done.text.as_str()) {
                    self.w.push(at, rest)?;
                } else {
                    // The done item states the whole text.
                    self.w.restate(at, &text)?;
                }
            }
            Item::Opaque => {}
        }
        let reasoning = done.kind == Item::Reasoning;
        self.w.edit(at, |stored| *stored = item)?;
        if reasoning {
            // Reasoning is sent only with the item it precedes, so it is
            // complete only once that item is.
            self.held.push(at);
            return Ok(());
        }
        self.w.finish(at)?;
        // A call with no name is dropped, so the reasoning it held has no
        // item to go with.
        self.release(!nameless)
    }

    fn item_event(&mut self, kind: &str, frame: &Value) -> Result<(), Error> {
        match kind {
            "response.output_item.added" | "response.output_item.done" => {
                let Some(item) = frame.get("item").filter(|item| item.is_object()) else {
                    return Ok(());
                };
                let index = output_index(frame)?;
                if kind == "response.output_item.added" {
                    return self.added(index, item).map(drop);
                }
                let known = self.addressed(frame, Item::of(item))?;
                // At a named output index the item is the one there,
                // whatever id it states.
                let restates = |slot: &Slot| {
                    slot.kind == Item::of(item)
                        && (index.is_some()
                            || item_id(item)
                                .zip(slot.id.as_deref())
                                .is_none_or(|(id, known)| id == known))
                };
                let known = known.and_then(|at| {
                    let slot = self.slots.get(at)?;
                    let same = item_id(item).is_some() && slot.id.as_deref() == item_id(item);
                    Some((at, slot.open && restates(slot), same))
                });
                let slot = match known {
                    Some((slot, true, _)) => slot,
                    // The item this one restates is already done.
                    Some((_, false, true)) => return Ok(()),
                    _ => self.added(index, item)?,
                };
                self.done(slot, item.clone())
            }
            // A refusal is the assistant's message for that turn.
            "response.output_text.delta" | "response.refusal.delta" => {
                self.text(frame, "message", "content_index")
            }
            "response.reasoning_summary_text.delta" => self.text(frame, "summary", "summary_index"),
            "response.reasoning_text.delta" => self.text(frame, "reasoning", "content_index"),
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                self.arguments(frame)
            }
            _ => Ok(()),
        }
    }

    /// The slot an item of the terminal's `output` restates.
    fn restated(
        &self,
        index: usize,
        item: &Value,
        output: &[Value],
        taken: &HashSet<usize>,
    ) -> Option<usize> {
        let id = item_id(item);
        let free = |at: &usize| {
            !taken.contains(at)
                && self
                    .slots
                    .get(*at)
                    .is_some_and(|slot| slot.kind == Item::of(item))
        };
        let find = |fits: &dyn Fn(usize, &Slot) -> bool| {
            self.slots
                .iter()
                .enumerate()
                .find(|(at, slot)| free(at) && fits(*at, slot))
                .map(|(at, _)| at)
        };
        id.and_then(|id| find(&|_, slot| slot.id.as_deref() == Some(id)))
            .or_else(|| self.indexed.get(&index).copied().filter(free))
            .or_else(|| {
                find(&|at, slot| {
                    !self.indexed.values().any(|indexed| *indexed == at)
                        && (id.is_none() || slot.id.is_none())
                })
            })
            .or_else(|| {
                find(&|at, slot| {
                    (id.is_none() || slot.id.is_none())
                        && self.indexed.iter().any(|(streamed, indexed)| {
                            *indexed == at
                                && output
                                    .get(*streamed)
                                    .is_none_or(|other| Item::of(other) != slot.kind)
                        })
                })
            })
    }

    fn finish(&mut self, response: &Value) -> Result<Flow, Error> {
        let output = response.arr("output");
        if self.slots.is_empty()
            && !output.iter().any(|item| Item::of(item) == Item::Reasoning)
            && let Some(reasoning) = response.str("reasoning").filter(|text| !text.is_empty())
        {
            // Index 0 orders it before the output; it closes at once, so the
            // output's first item may open there after it.
            self.w.whole(
                0,
                Open::Reasoning { redacted: false },
                Value::Null,
                reasoning,
            )?;
        }
        let mut taken = HashSet::new();
        for (index, item) in output
            .iter()
            .enumerate()
            .filter(|(_, item)| item.is_object())
        {
            let known = self.restated(index, item, output, &taken).and_then(|at| {
                let slot = self.slots.get(at)?;
                Some((at, slot.open, slot.kind, slot.at))
            });
            let slot = match known {
                Some((slot, false, kind, at)) => {
                    taken.insert(slot);
                    let ciphertext = item
                        .get("encrypted_content")
                        .filter(|cipher| cipher.as_str().is_some_and(|cipher| !cipher.is_empty()));
                    if let (Item::Reasoning, Some(ciphertext)) = (kind, ciphertext) {
                        self.w.edit(at, |stored| {
                            let stated = stored
                                .str("encrypted_content")
                                .is_some_and(|cipher| !cipher.is_empty());
                            if let (false, Some(fields)) = (stated, stored.as_object_mut()) {
                                fields.insert("encrypted_content".to_owned(), ciphertext.clone());
                            }
                        })?;
                    }
                    continue;
                }
                Some((slot, ..)) => slot,
                None => self.added(
                    Some(index).filter(|index| !self.indexed.contains_key(index)),
                    item,
                )?,
            };
            taken.insert(slot);
            self.done(slot, item.clone())?;
        }
        // Reasoning still waiting is complete when nothing after it is left
        // open.
        let open = self.slots.iter().any(|slot| slot.open);
        self.release(!open)?;
        // A call never done shows what streamed, in a turn that fails.
        for slot in self.slots.iter_mut().filter(|slot| slot.open) {
            if slot.kind == Item::Call {
                flush(slot, &mut self.w)?;
            }
        }
        let (reason, raw, message) = finish_of(response);
        let usage = Usage {
            // Hosted tools report their requests beside the usage.
            hosted_tool_calls: super::super::counters(
                response.get("tool_usage"),
                Some("num_requests"),
            )
            .filter(|calls| *calls > 0),
            ..response.get("usage").map_or_default(usage_of)
        };
        self.w.end(End {
            usage,
            reason: Some((reason, raw)),
            response_id: item_id(response).map(str::to_owned),
            model: response.str("model").map(str::to_owned),
            message,
            meta: super::super::usage_meta(KEY, response.get("usage")),
        })
    }

    fn decode(&mut self, data: &str) -> Result<Flow, Error> {
        if data.trim() == "[DONE]" {
            // The provider's end is `response.completed`.
            return Ok(Flow::More);
        }
        let frame: Value =
            serde_json::from_str(data).map_err(|error| Error::decode(error, data))?;
        if !frame.is_object() {
            return Ok(Flow::More);
        }
        let Some(kind) = frame.str("type") else {
            // An untagged body: the whole response, or its error.
            let failed = frame.get("error").is_some_and(|error| !error.is_null());
            if failed && frame.get("output").is_none() && frame.get("status").is_none() {
                return Err(failure(&frame));
            }
            if ["object", "output", "status", "id"]
                .iter()
                .any(|key| frame.get(*key).is_some())
            {
                return self.finish(&frame);
            }
            return Ok(Flow::More);
        };
        if !is_known(kind) {
            return Ok(Flow::More);
        }
        match kind {
            // `response.incomplete` is a terminal that keeps the partial
            // output and usage, incomplete whatever its `status` says.
            "response.completed" | "response.incomplete" => {
                let mut response = frame
                    .get("response")
                    .filter(|response| response.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if let (true, Some(fields)) =
                    (kind == "response.incomplete", response.as_object_mut())
                {
                    fields.insert("status".to_owned(), json!("incomplete"));
                }
                self.finish(&response)
            }
            "error" | "response.failed" => Err(failure(&frame)),
            kind if is_lifecycle(kind) => Ok(Flow::More),
            kind => {
                let kind = kind.to_owned();
                self.item_event(&kind, &frame)?;
                Ok(Flow::More)
            }
        }
    }
}

impl Decoder for ResponsesDecoder {
    fn frame(&mut self, frame: Frame<'_>, out: &mut Vec<Event>) -> Result<Flow, Error> {
        let flow = self.decode(frame.data);
        self.w.drain(out);
        flow
    }
}

/// Write the argument text the writer does not have yet.
fn send(slot: &mut Slot, w: &mut Writer) -> Result<(), Error> {
    if slot.arguments.is_empty() {
        return Ok(());
    }
    let streamed = if slot.custom {
        let quoted = Value::String(slot.arguments.clone()).to_string();
        // The string without its closing quote: later input extends it.
        format!("{{\"input\":{}", &quoted[..quoted.len() - 1])
    } else {
        slot.arguments.clone()
    };
    let Some(rest) = streamed
        .strip_prefix(slot.sent.as_str())
        .filter(|rest| !rest.is_empty())
    else {
        return Ok(());
    };
    w.fragment(
        Some(slot.at),
        CallFragment {
            arguments: Some(rest),
            ..CallFragment::default()
        },
    )?;
    slot.sent = streamed;
    Ok(())
}

fn flush(slot: &mut Slot, w: &mut Writer) -> Result<(), Error> {
    let arguments = streamed_arguments(slot);
    let rest = remainder(slot, &arguments, w)?;
    w.fragment(
        Some(slot.at),
        CallFragment {
            arguments: Some(&rest),
            ..CallFragment::default()
        },
    )
}

/// What `arguments` adds to what the writer has; a call whose text the
/// provider restated differently is restated whole.
fn remainder(slot: &mut Slot, arguments: &str, w: &mut Writer) -> Result<String, Error> {
    let rest = if let Some(rest) = arguments.strip_prefix(slot.sent.as_str()) {
        rest.to_owned()
    } else {
        w.restate(slot.at, arguments)?;
        String::new()
    };
    arguments.clone_into(&mut slot.sent);
    Ok(rest)
}

fn streamed_arguments(slot: &Slot) -> String {
    if slot.custom {
        json!({ "input": slot.arguments }).to_string()
    } else {
        slot.arguments.clone()
    }
}

fn output_index(frame: &Value) -> Result<Option<usize>, Error> {
    frame
        .u64("output_index")
        .map(|index| {
            usize::try_from(index).map_err(|_| {
                Error::new(
                    Kind::Decode,
                    format!("output_index {index} is out of range"),
                )
            })
        })
        .transpose()
}
