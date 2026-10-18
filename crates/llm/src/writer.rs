//! What a decoder writes through: provider items by wire index, turned into
//! [`Event`]s with the rules every wire shares (ported from rig's completion
//! fold). A decoder states what the provider sent; the writer decides what
//! a consumer sees.
//!
//! - A part opens to the consumer at its first content, so empty text and
//!   reasoning never appear; a call opens once it names its tool, and one
//!   that never does is dropped.
//! - A call's arguments are parsed at its end and never fail the reply; a
//!   call with no id, or one an earlier call took, gets a fresh id.
//! - Provider items become part state only when the provider stated them
//!   complete, and only for parts before the first one that never
//!   completed: a later item might need a partner that never arrived.
//! - A reply the provider ended without a finish reason, or with a call it
//!   never finished, is incomplete.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Map, Value, json};

use crate::error::Error;
use crate::replay::{Canon, fingerprint};
use crate::spec::{
    AssistantPart, Block, CallEnd, Event, Finish, Media, Meta, Opaque, Reason, Usage,
};

/// The first index the writer hands out itself ([`Writer::fresh_index`]);
/// a provider's wire indices stay below it.
pub const AUTO_INDEX: usize = 1 << 30;

/// The most argument bytes one call accumulates.
const MAX_ARGUMENTS: usize = 32 * 1024 * 1024;

/// What a provider item becomes when it opens.
#[derive(Clone, Debug, PartialEq)]
pub enum Open {
    Text,
    Reasoning {
        redacted: bool,
    },
    Call {
        id: String,
        name: String,
    },
    /// An item with no shared meaning; `replay` says whether it goes back
    /// to the model that produced it.
    Opaque {
        replay: bool,
    },
    /// Media the model produced, whole when it opens; it keeps no provider
    /// item.
    Media(Media),
}

/// One fragment of a call a provider streams in pieces.
#[derive(Clone, Copy, Debug, Default)]
pub struct CallFragment<'a> {
    pub id: Option<&'a str>,
    pub name: Option<&'a str>,
    pub arguments: Option<&'a str>,
}

/// What the provider says when it ends the reply.
#[derive(Clone, Debug, Default)]
pub struct End {
    pub usage: Usage,
    /// The normalized reason and the provider's word for it.
    pub reason: Option<(Reason, Option<String>)>,
    pub response_id: Option<String>,
    pub model: Option<String>,
    /// The provider's account of a turn that went wrong: a refusal's
    /// explanation, why a reply is incomplete.
    pub message: Option<String>,
    /// Reply-level provider state.
    pub meta: Meta,
}

pub struct Writer {
    key: &'static str,
    out: Vec<Event>,
    open: BTreeMap<usize, Draft>,
    /// The part each wire index last closed, whose item may still be edited.
    ended: HashMap<usize, u32>,
    /// Provider items of parts that closed, by part, with the fingerprint of
    /// the shared fields they were decoded to.
    items: BTreeMap<u32, (Value, String)>,
    /// Opaque parts and whether they replay.
    opaque: BTreeMap<u32, bool>,
    /// Every part emitted, in emission order.
    parts: Vec<u32>,
    next_part: u32,
    call_ids: HashSet<String>,
    last_call: Option<usize>,
    run: Option<usize>,
    next_auto: usize,
    by_index: Option<BTreeMap<u32, usize>>,
    lead: Option<u32>,
    first_incomplete: Option<u32>,
    unfinished_call: bool,
    states_finish_reason: bool,
    /// Whether a call item holds its id in a slot replay can fill, so a call
    /// sent with no id keeps its item.
    call_id_slot: bool,
    /// Seeds the ids the writer issues.
    ids: u64,
}

struct Draft {
    part: u32,
    started: bool,
    item: Value,
    body: Body,
    /// The text or reasoning streamed so far, to tell a restatement apart.
    sent: String,
}

enum Body {
    Text(String),
    Reasoning {
        text: String,
        redacted: bool,
    },
    Call {
        id: Option<String>,
        name: String,
        arguments: Arguments,
    },
    Opaque {
        replay: bool,
    },
    Media(Media),
}

#[derive(Default)]
struct Arguments {
    text: String,
    /// How many bytes of `text` the stream has carried.
    streamed: usize,
    overflowed: bool,
    /// Arguments announced when the call opened, used only when no
    /// fragment arrives.
    announced: Option<Value>,
}

impl Arguments {
    fn push(&mut self, fragment: &str) {
        // Some gateways send a literal `null` before the real fragments; a
        // non-blank fragment supersedes it.
        if self.text.trim() == "null" && !fragment.trim().is_empty() {
            self.text.clear();
        }
        if self.text.len().saturating_add(fragment.len()) > MAX_ARGUMENTS {
            self.overflowed = true;
        } else {
            self.text.push_str(fragment);
        }
    }

    fn end(&self) -> (Map<String, Value>, Option<String>) {
        if self.text.is_empty()
            && let Some(Value::Object(announced)) = &self.announced
        {
            return (announced.clone(), None);
        }
        let (input, invalid) = crate::json::arguments(&self.text);
        let invalid = invalid.or_else(|| self.overflowed.then(|| self.text.clone()));
        (input, invalid)
    }

    /// The text the stream has not carried yet, now marked carried. Text
    /// that may still be a placeholder `null` waits.
    fn unstreamed(&mut self) -> Option<String> {
        if "null".starts_with(self.text.trim()) {
            return None;
        }
        let fragment = self.text.get(self.streamed..)?;
        if fragment.is_empty() {
            return None;
        }
        let fragment = fragment.to_owned();
        self.streamed = self.text.len();
        Some(fragment)
    }

    /// Whether the text is a complete JSON object.
    fn complete(&self) -> bool {
        !self.overflowed && matches!(serde_json::from_str(&self.text), Ok(Value::Object(_)))
    }
}

/// Whether `id` names a call: some providers send `""` or `"null"` for a
/// call they gave no id.
fn stated(id: &str) -> bool {
    !id.is_empty() && id != "null"
}

fn not_open(index: usize) -> Error {
    Error::decode(
        format!("the reply wrote to item {index}, which is not open"),
        "",
    )
}

impl Writer {
    /// A writer for a protocol whose part state goes under `key`.
    pub fn new(key: &'static str) -> Self {
        Self {
            key,
            out: Vec::new(),
            open: BTreeMap::new(),
            ended: HashMap::new(),
            items: BTreeMap::new(),
            opaque: BTreeMap::new(),
            parts: Vec::new(),
            next_part: 0,
            call_ids: HashSet::new(),
            last_call: None,
            run: None,
            next_auto: AUTO_INDEX,
            by_index: None,
            lead: None,
            first_incomplete: None,
            unfinished_call: false,
            states_finish_reason: true,
            call_id_slot: false,
            ids: seed(),
        }
    }

    /// A wire whose end need not name a finish reason.
    #[must_use]
    pub fn without_finish_reason(mut self) -> Self {
        self.states_finish_reason = false;
        self
    }

    /// A wire whose call items hold their id in a slot replay can fill.
    #[must_use]
    pub fn with_call_id_slot(mut self) -> Self {
        self.call_id_slot = true;
        self
    }

    /// Moves the events written so far to `out`.
    pub fn drain(&mut self, out: &mut Vec<Event>) {
        out.append(&mut self.out);
    }

    fn emit(&mut self, event: Event) {
        self.out.push(event);
    }

    fn next(&mut self) -> u32 {
        let part = self.next_part;
        self.next_part += 1;
        part
    }

    pub fn fresh_index(&mut self) -> usize {
        let index = self.next_auto;
        self.next_auto += 1;
        index
    }

    pub fn is_open(&self, index: usize) -> bool {
        self.open.contains_key(&index)
    }

    /// Order the message's parts by wire index rather than by when they
    /// opened, for a wire whose end may state items it never streamed.
    pub fn order_by_index(&mut self) {
        self.by_index.get_or_insert_with(BTreeMap::new);
    }

    /// Hold the open item at `index` first in the message.
    pub fn lead(&mut self, index: usize) -> Result<(), Error> {
        let part = self.draft(index)?.part;
        self.lead.get_or_insert(part);
        Ok(())
    }

    fn draft(&mut self, index: usize) -> Result<&mut Draft, Error> {
        self.open.get_mut(&index).ok_or_else(|| not_open(index))
    }

    /// Open the provider item at wire `index`. `item` is the item as its
    /// first event states it, the base its deltas merge into; `Null` when
    /// the part keeps no provider item.
    pub fn open(&mut self, index: usize, block: Open, item: Value) -> Result<(), Error> {
        if self.open.contains_key(&index) {
            return Err(Error::decode(
                format!("the reply opened item {index} twice"),
                "",
            ));
        }
        let body = match block {
            Open::Text => Body::Text(String::new()),
            Open::Reasoning { redacted } => Body::Reasoning {
                text: String::new(),
                redacted,
            },
            Open::Call { id, name } => Body::Call {
                id: Some(id).filter(|id| stated(id)),
                name,
                arguments: Arguments::default(),
            },
            Open::Opaque { replay } => Body::Opaque { replay },
            Open::Media(media) => Body::Media(media),
        };
        self.insert(index, body, item);
        self.stream_call(index);
        Ok(())
    }

    fn insert(&mut self, index: usize, body: Body, item: Value) {
        self.ended.remove(&index);
        let part = self.next();
        if let Some(by_index) = &mut self.by_index {
            by_index.insert(part, index);
        }
        self.open.insert(
            index,
            Draft {
                part,
                started: false,
                item,
                body,
                sent: String::new(),
            },
        );
    }

    /// Append text, reasoning, or a call's argument JSON to the item at
    /// `index`.
    pub fn push(&mut self, index: usize, fragment: &str) -> Result<(), Error> {
        if fragment.is_empty() {
            return Ok(());
        }
        let draft = self.open.get_mut(&index).ok_or_else(|| not_open(index))?;
        let block = match &mut draft.body {
            Body::Text(text) => {
                text.push_str(fragment);
                Block::Text
            }
            Body::Reasoning { text, .. } => {
                text.push_str(fragment);
                Block::Reasoning
            }
            Body::Call { arguments, .. } => {
                arguments.push(fragment);
                self.stream_call(index);
                return Ok(());
            }
            Body::Opaque { .. } | Body::Media(_) => {
                return Err(Error::decode(
                    format!("the reply wrote text to the whole item {index}"),
                    fragment,
                ));
            }
        };
        draft.sent.push_str(fragment);
        let part = draft.part;
        if !draft.started {
            draft.started = true;
            self.parts.push(part);
            self.emit(Event::Start {
                index: part,
                block,
                name: None,
            });
        }
        self.emit(Event::Delta {
            index: part,
            text: fragment.to_owned(),
        });
        Ok(())
    }

    /// Edit the item at `index` in place: merge a delta, or replace the
    /// whole item the provider restates when it finishes. An index whose
    /// part already closed edits that part's item until the reply ends.
    pub fn edit(&mut self, index: usize, edit: impl FnOnce(&mut Value)) -> Result<(), Error> {
        if let Some(draft) = self.open.get_mut(&index) {
            edit(&mut draft.item);
            return Ok(());
        }
        let part = *self.ended.get(&index).ok_or_else(|| not_open(index))?;
        if let Some((item, _)) = self.items.get_mut(&part) {
            edit(item);
        }
        Ok(())
    }

    /// Replace the text, reasoning or argument JSON of the open item at
    /// `index` with the whole of it as the provider restates it. What
    /// streamed stands; the part ends holding `text`.
    pub fn restate(&mut self, index: usize, text: &str) -> Result<(), Error> {
        let draft = self.draft(index)?;
        match &mut draft.body {
            Body::Text(body) | Body::Reasoning { text: body, .. } => text.clone_into(body),
            Body::Call { arguments, .. } => {
                text.clone_into(&mut arguments.text);
                // A call not yet started streams the restated text whole.
                arguments.streamed = if draft.started { text.len() } else { 0 };
            }
            Body::Opaque { .. } | Body::Media(_) => {}
        }
        Ok(())
    }

    /// Close the item at `index` the provider never stated complete: its
    /// part keeps no provider item, and an opaque one does not replay.
    pub fn close(&mut self, index: usize) -> Result<(), Error> {
        self.close_item(index, false)
    }

    /// Close the item at `index` the provider stated complete: its item as
    /// assembled becomes the part's state.
    pub fn finish(&mut self, index: usize) -> Result<(), Error> {
        self.close_item(index, true)
    }

    /// Finish every item still open, for a wire whose end of reply states
    /// its items done.
    pub fn finish_open(&mut self) -> Result<(), Error> {
        let mut open: Vec<(u32, usize)> = self
            .open
            .iter()
            .map(|(index, draft)| (draft.part, *index))
            .collect();
        open.sort_unstable();
        for (_, index) in open {
            self.close_item(index, true)?;
        }
        Ok(())
    }

    /// Open and finish the item at `index` in one step.
    pub fn whole(
        &mut self,
        index: usize,
        block: Open,
        item: Value,
        text: &str,
    ) -> Result<(), Error> {
        self.open(index, block, item)?;
        self.push(index, text)?;
        self.finish(index)
    }

    /// Arguments the provider announced for the call at `index`, used only
    /// if no fragment arrives.
    pub fn announce(&mut self, index: usize, announced: Value) -> Result<(), Error> {
        if let Body::Call { arguments, .. } = &mut self.draft(index)?.body {
            arguments.announced = Some(announced);
        }
        Ok(())
    }

    /// One fragment of a call the provider streams, opening the call at its
    /// first fragment. Its id and name may arrive in any fragment, and the
    /// first name it is given stays. Calls are told apart by `index` when
    /// the wire gives one: a new id under an index starts a new call once
    /// the held call's arguments are a complete object or the new fragment
    /// names a tool (some providers send a fresh id, and no name, with every
    /// chunk of one call). Without an index, a fragment with an unseen id
    /// opens a call, and one with no id continues the latest call while its
    /// arguments are incomplete.
    pub fn fragment(
        &mut self,
        index: Option<usize>,
        fragment: CallFragment<'_>,
    ) -> Result<(), Error> {
        let new_id = fragment.id.filter(|id| stated(id));
        let index = match index {
            Some(index) => self.indexed_call(index, new_id, fragment.name)?,
            None => self.unindexed_call(new_id),
        };
        self.last_call = Some(index);
        if !self.open.contains_key(&index) {
            let body = Body::Call {
                id: None,
                name: String::new(),
                arguments: Arguments::default(),
            };
            self.insert(index, body, Value::Null);
        }
        let Body::Call {
            id,
            name,
            arguments,
        } = &mut self.draft(index)?.body
        else {
            return Err(Error::decode(
                format!("the reply sent a call fragment for item {index}, which is not a call"),
                "",
            ));
        };
        if let Some(call_id) = new_id {
            *id = Some(call_id.to_owned());
        }
        if name.is_empty()
            && let Some(fragment) = fragment.name.filter(|name| !name.is_empty())
        {
            fragment.clone_into(name);
        }
        if let Some(fragment) = fragment.arguments {
            arguments.push(fragment);
        }
        self.stream_call(index);
        Ok(())
    }

    /// The call a fragment under wire `index` writes to. When another call
    /// takes over the index, the one it held ends there.
    fn indexed_call(
        &mut self,
        index: usize,
        new_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<usize, Error> {
        let names = name.is_some_and(|name| !name.is_empty());
        let held = self.open.get(&index).and_then(|draft| match &draft.body {
            Body::Call {
                id: Some(id),
                arguments,
                ..
            } if new_id.is_some_and(|new| id != new) => Some(arguments.complete()),
            _ => None,
        });
        if let Some(complete) = held.filter(|complete| *complete || names) {
            let moved = self.fresh_index();
            if let Some(draft) = self.open.remove(&index) {
                self.open.insert(moved, draft);
            }
            self.close_item(moved, complete)?;
        }
        Ok(index)
    }

    /// The call an index-less fragment writes to: the open call with its id,
    /// else the latest call while its arguments are incomplete, else a new one.
    fn unindexed_call(&mut self, new_id: Option<&str>) -> usize {
        let owner = new_id.and_then(|new| {
            self.open
                .iter()
                .find_map(|(index, draft)| match &draft.body {
                    Body::Call { id: Some(id), .. } if id == new => Some(*index),
                    _ => None,
                })
        });
        let continues = |last: &usize| {
            self.open.get(last).is_some_and(|draft| {
                matches!(&draft.body, Body::Call { arguments, .. } if !arguments.complete())
            })
        };
        match (owner, new_id, self.last_call) {
            (Some(index), _, _) => index,
            (None, None, Some(last)) if continues(&last) => last,
            _ => self.fresh_index(),
        }
    }

    /// A fragment on a wire that marks no item boundaries: it continues the
    /// part the last fragment went to while `block` is the same kind, and
    /// otherwise closes that part and opens a new one. Returns its index.
    pub fn run(&mut self, block: Open, fragment: &str) -> Result<usize, Error> {
        let current = self.run.filter(|index| {
            self.open.get(index).is_some_and(|draft| {
                matches!(
                    (&draft.body, &block),
                    (Body::Text(_), Open::Text)
                        | (
                            Body::Reasoning {
                                redacted: false,
                                ..
                            },
                            Open::Reasoning { redacted: false }
                        )
                )
            })
        });
        let index = if let Some(index) = current {
            index
        } else {
            self.end_run()?;
            let index = self.fresh_index();
            self.open(index, block, Value::Null)?;
            self.run = Some(index);
            index
        };
        self.push(index, fragment)?;
        Ok(index)
    }

    /// Close the part [`Self::run`] is extending: output of another kind
    /// interleaved it.
    pub fn end_run(&mut self) -> Result<(), Error> {
        match self.run.take() {
            Some(index) => self.close_item(index, true),
            None => Ok(()),
        }
    }

    /// Carry what the open call at `index` has not streamed yet: its start
    /// once it names a tool, then the argument text since the last fragment.
    fn stream_call(&mut self, index: usize) {
        let Some(draft) = self.open.get_mut(&index) else {
            return;
        };
        let part = draft.part;
        let Body::Call {
            name, arguments, ..
        } = &mut draft.body
        else {
            return;
        };
        let mut events = Vec::new();
        if !draft.started {
            if name.is_empty() {
                return;
            }
            draft.started = true;
            self.parts.push(part);
            events.push(Event::Start {
                index: part,
                block: Block::ToolCall,
                name: Some(name.clone()),
            });
        }
        if let Some(text) = arguments.unstreamed() {
            events.push(Event::Delta { index: part, text });
        }
        self.out.extend(events);
    }

    /// The id a closing call takes, and the item it keeps: its own, or a
    /// fresh one when the provider sent none or one an earlier call holds.
    /// A renamed call keeps no item, since the item names the id it lost.
    fn distinct_call_id(&mut self, id: Option<String>, item: Value) -> (String, Value) {
        let (id, item) = match id {
            Some(id) if !self.call_ids.contains(&id) => (id, item),
            None if self.call_id_slot => (self.local_id(), item),
            Some(_) | None => (self.local_id(), Value::Null),
        };
        self.call_ids.insert(id.clone());
        (id, item)
    }

    fn local_id(&mut self) -> String {
        loop {
            self.ids = self.ids.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let id = format!("call_{:016x}", self.ids);
            if !self.call_ids.contains(&id) {
                return id;
            }
        }
    }

    /// Emit a part the provider stated whole.
    fn whole_part(&mut self, part: u32, whole: AssistantPart) {
        self.parts.push(part);
        self.emit(Event::Part {
            index: part,
            part: whole,
        });
    }

    fn close_item(&mut self, index: usize, complete: bool) -> Result<(), Error> {
        self.stream_call(index);
        let draft = self.open.remove(&index).ok_or_else(|| not_open(index))?;
        if self.run == Some(index) {
            self.run = None;
        }
        self.ended.insert(index, draft.part);
        let Draft {
            part,
            started,
            item,
            body,
            sent,
        } = draft;
        let item = match (complete, &body) {
            (true, _) | (false, Body::Opaque { .. } | Body::Media(_)) => item,
            (false, _) => {
                self.first_incomplete =
                    Some(self.first_incomplete.map_or(part, |first| first.min(part)));
                Value::Null
            }
        };
        let (block, redacted, text) = match body {
            Body::Text(text) => {
                if text.is_empty() && item.is_null() {
                    return Ok(());
                }
                (Block::Text, false, text)
            }
            Body::Reasoning { text, redacted } => {
                if text.is_empty() && !redacted && item.is_null() {
                    return Ok(());
                }
                (Block::Reasoning, redacted, text)
            }
            Body::Opaque { replay } => {
                self.opaque.insert(part, replay && complete);
                self.items.insert(part, (item, String::new()));
                self.whole_part(part, AssistantPart::Opaque(Opaque { meta: Meta::new() }));
                return Ok(());
            }
            Body::Media(media) => {
                self.whole_part(part, AssistantPart::Media(media));
                return Ok(());
            }
            Body::Call {
                id,
                name,
                arguments,
            } => {
                if name.is_empty() {
                    return Ok(());
                }
                let (input, invalid) = arguments.end();
                // A call that streamed no text (none arrived, or only a
                // placeholder `null`) states the arguments it ends with.
                if arguments.streamed == 0 {
                    self.emit(Event::Delta {
                        index: part,
                        text: Value::Object(input.clone()).to_string(),
                    });
                }
                let (id, item) = self.distinct_call_id(id, item);
                if !item.is_null() {
                    let fp = fingerprint(Canon::Call(&name, &input));
                    self.items.insert(part, (item, fp));
                }
                self.emit(Event::End {
                    index: part,
                    redacted: false,
                    text: None,
                    call: Some(CallEnd {
                        id,
                        input,
                        invalid,
                        provider_executed: false,
                    }),
                });
                return Ok(());
            }
        };
        if !item.is_null() {
            let canon = match block {
                Block::Reasoning => Canon::Reasoning(&text, redacted),
                _ => Canon::Text(&text),
            };
            self.items.insert(part, (item, fingerprint(canon)));
        }
        if !started {
            self.parts.push(part);
            self.emit(Event::Start {
                index: part,
                block,
                name: None,
            });
        }
        // A restatement replaces what streamed.
        self.emit(Event::End {
            index: part,
            redacted,
            text: (text != sent).then_some(text),
            call: None,
        });
        Ok(())
    }

    /// The reply ends: what is still open closes, the parts' provider state
    /// is attached, and the finish follows. A wire whose end must name a
    /// reason fails a reply that names none, or that ends with a call left
    /// open.
    pub fn end(&mut self, end: End) -> Result<crate::protocol::Flow, Error> {
        let mut open: Vec<(u32, usize)> = self
            .open
            .iter()
            .map(|(index, draft)| (draft.part, *index))
            .collect();
        open.sort_unstable();
        for (_, index) in open {
            let complete = self.run == Some(index);
            if matches!(
                self.open.get(&index).map(|draft| &draft.body),
                Some(Body::Call { .. })
            ) {
                self.unfinished_call = true;
            }
            self.close_item(index, complete)?;
        }
        if self.states_finish_reason {
            match &end.reason {
                None => {
                    return Err(Error::new(
                        crate::Kind::Incomplete,
                        "the provider ended the reply without a finish reason",
                    ));
                }
                Some((Reason::Length, _)) => {}
                Some(_) if self.unfinished_call => {
                    return Err(Error::new(
                        crate::Kind::Incomplete,
                        "the provider ended the reply with a tool call it never finished",
                    ));
                }
                Some(_) => {}
            }
        }
        if end.response_id.is_some() || end.model.is_some() {
            self.emit(Event::Metadata {
                id: end.response_id.filter(|id| !id.is_empty()),
                model: end.model.filter(|model| !model.is_empty()),
            });
        }
        let canonical_from = self.first_incomplete.unwrap_or(u32::MAX);
        let items = std::mem::take(&mut self.items);
        for (part, (item, fp)) in items {
            let replay = self.opaque.get(&part).copied();
            if part >= canonical_from && replay.is_none() {
                continue;
            }
            let mut state = json!({ "item": item });
            match replay {
                Some(replay) => state["replay"] = Value::Bool(replay && part < canonical_from),
                None => state["fp"] = Value::String(fp),
            }
            self.emit(Event::Meta {
                index: part,
                meta: Meta::from([(self.key.to_owned(), state)]),
            });
        }
        let has_call = !self.call_ids.is_empty();
        let (reason, raw) = match end.reason {
            Some((Reason::Stop, raw)) if has_call => (Reason::ToolCalls, raw),
            Some((reason, raw)) => (reason, raw),
            None => (Reason::Stop, None),
        };
        let order = self.order();
        self.emit(Event::Finish {
            finish: Finish {
                reason,
                raw,
                message: end.message,
            },
            usage: end.usage,
            meta: end.meta,
            order,
        });
        Ok(crate::protocol::Flow::Done)
    }

    /// The parts' message order, when it is not the order they opened in.
    fn order(&self) -> Option<Vec<u32>> {
        if self.by_index.is_none() && self.lead.is_none() {
            return None;
        }
        let mut parts = self.parts.clone();
        if let Some(by_index) = &self.by_index {
            parts.sort_by_key(|part| (by_index.get(part).copied().unwrap_or(usize::MAX), *part));
        }
        if let Some(lead) = self.lead {
            parts.sort_by_key(|part| *part != lead);
        }
        let mut natural = self.parts.clone();
        natural.sort_unstable();
        (parts != natural).then_some(parts)
    }
}

/// `delta` merged into `item`: each string field but `type` appends to the
/// item's string of that key, an array extends the item's array, and any
/// other value replaces the key.
pub fn merge(item: &mut Value, delta: &Map<String, Value>) {
    if !item.is_object() {
        *item = Value::Object(Map::new());
    }
    let Value::Object(item) = item else {
        return;
    };
    for (key, value) in delta {
        if key == "type" {
            continue;
        }
        match (item.get_mut(key), value) {
            (Some(Value::String(existing)), Value::String(fragment)) => existing.push_str(fragment),
            (Some(Value::Array(existing)), Value::Array(more)) => {
                existing.extend(more.iter().cloned());
            }
            _ => {
                item.insert(key.clone(), value.clone());
            }
        }
    }
}

/// A per-process seed for writer-issued call ids.
fn seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos().into()),
    );
    hasher.finish()
}

#[cfg(test)]
mod tests;
