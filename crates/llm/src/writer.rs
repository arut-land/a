//! What a decoder writes through: provider items by wire index, turned into
//! [`Event`]s with the rules every wire shares. A decoder states what the
//! provider sent; the writer decides what a consumer sees.
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

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::error::{Error, Kind};
use crate::protocol::{Decoder, Flow, Frame};
use crate::replay::{Canon, fingerprint};
use crate::spec::{
    AssistantPart, Block, CallEnd, Event, Finish, Media, Meta, Opaque, Reason, SmolStr, Usage,
};

/// The first index the writer hands out itself ([`Writer::fresh_index`]);
/// a provider's wire indices stay below it.
pub const AUTO_INDEX: usize = 1 << 30;

/// The most argument bytes one call accumulates.
const MAX_ARGUMENTS: usize = 32 * 1024 * 1024;

/// A decoder that writes through a [`Writer`]: what each frame wrote
/// reaches the consumer once the frame is decoded.
pub trait Writes: Send {
    fn writer(&mut self) -> &mut Writer;

    /// Decode one frame's data.
    fn data(&mut self, data: &str) -> Result<Flow, Error>;

    /// The body ended without [`Flow::Done`]; see [`Decoder::end`].
    fn ended(&mut self) -> Result<(), Error> {
        Err(Error::incomplete())
    }
}

impl<T: Writes> Decoder for T {
    fn frame(&mut self, frame: Frame<'_>, out: &mut Vec<Event>) -> Result<Flow, Error> {
        let flow = self.data(frame.data);
        self.writer().drain(out);
        flow
    }

    fn end(&mut self, out: &mut Vec<Event>) -> Result<(), Error> {
        let ended = self.ended();
        self.writer().drain(out);
        ended
    }
}

/// What a provider item becomes when it opens.
#[derive(Clone, Debug, PartialEq)]
pub enum Open {
    Text,
    Reasoning {
        redacted: bool,
    },
    Call {
        id: SmolStr,
        name: SmolStr,
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
    pub reason: Option<(Reason, Option<SmolStr>)>,
    pub response_id: Option<SmolStr>,
    pub model: Option<SmolStr>,
    /// The provider's account of a turn that went wrong: a refusal's
    /// explanation, why a reply is incomplete.
    pub message: Option<String>,
    /// The provider's own usage object, kept on the reply's meta.
    pub raw_usage: Option<Value>,
}

pub struct Writer {
    key: &'static str,
    out: Vec<Event>,
    open: BTreeMap<usize, Draft>,
    /// The part each wire index last closed, whose item may still be edited.
    /// A reply holds few, and a list costs less code than a map.
    ended: Vec<(usize, u32)>,
    /// Provider items of parts that closed, by part, with the fingerprint of
    /// the shared fields they were decoded to.
    items: BTreeMap<u32, (Value, String)>,
    /// Opaque parts and whether they replay.
    opaque: BTreeMap<u32, bool>,
    /// Every part emitted, in emission order.
    parts: Vec<u32>,
    next_part: u32,
    /// The ids calls ended with, each once.
    call_ids: Vec<SmolStr>,
    next_auto: usize,
    by_index: Option<BTreeMap<u32, usize>>,
    lead: Option<u32>,
    first_incomplete: Option<u32>,
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
    /// Whether the provider restated the text or reasoning, so the part
    /// ends holding something other than what streamed.
    restated: bool,
}

enum Body {
    Text(String),
    Reasoning {
        text: String,
        redacted: bool,
    },
    Call {
        id: Option<SmolStr>,
        name: SmolStr,
        arguments: Arguments,
    },
    Opaque {
        replay: bool,
    },
    Media(Media),
}

impl From<Open> for Body {
    fn from(open: Open) -> Self {
        match open {
            Open::Text => Self::Text(String::new()),
            Open::Reasoning { redacted } => Self::Reasoning {
                text: String::new(),
                redacted,
            },
            Open::Call { id, name } => Self::Call {
                id: Some(id).filter(|id| stated(id)),
                name,
                arguments: Arguments::default(),
            },
            Open::Opaque { replay } => Self::Opaque { replay },
            Open::Media(media) => Self::Media(media),
        }
    }
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

    fn end(&mut self) -> (Map<String, Value>, Option<String>) {
        if self.text.is_empty()
            && let Some(Value::Object(announced)) = self.announced.take()
        {
            return (announced, None);
        }
        let (input, invalid) = crate::json::arguments(&self.text);
        let invalid = invalid.or_else(|| self.overflowed.then(|| std::mem::take(&mut self.text)));
        (input, invalid)
    }

    /// The text the stream has not carried yet, now marked carried. Text
    /// that may still be a placeholder `null` waits.
    fn unstreamed(&mut self) -> Option<String> {
        if "null".starts_with(self.text.trim()) {
            return None;
        }
        let fragment = self
            .text
            .get(self.streamed..)
            .filter(|rest| !rest.is_empty())?
            .to_owned();
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
            ended: Vec::new(),
            items: BTreeMap::new(),
            opaque: BTreeMap::new(),
            parts: Vec::new(),
            next_part: 0,
            call_ids: Vec::new(),
            next_auto: AUTO_INDEX,
            by_index: None,
            lead: None,
            first_incomplete: None,
            call_id_slot: false,
            ids: seed(),
        }
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

    pub fn fresh_index(&mut self) -> usize {
        self.next_auto += 1;
        self.next_auto - 1
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

    /// Whether an item opened at `index`, closed since or not.
    pub fn opened(&self, index: usize) -> bool {
        self.open.contains_key(&index) || self.closed(index).is_some()
    }

    /// The part wire `index` last closed.
    fn closed(&self, index: usize) -> Option<u32> {
        let mut ended = self.ended.iter();
        ended.find(|(at, _)| *at == index).map(|(_, part)| *part)
    }

    /// The text, reasoning or argument JSON the open item at `index` holds
    /// so far; `None` for an item of none.
    pub fn text(&self, index: usize) -> Option<&str> {
        match &self.open.get(&index)?.body {
            Body::Text(text) | Body::Reasoning { text, .. } => Some(text),
            Body::Call { arguments, .. } => Some(&arguments.text),
            Body::Opaque { .. } | Body::Media(_) => None,
        }
    }

    /// What the open call at `index` holds so far: its id and tool name
    /// once stated, and its argument JSON.
    pub fn call(&self, index: usize) -> Option<CallFragment<'_>> {
        match &self.open.get(&index)?.body {
            Body::Call {
                id,
                name,
                arguments,
            } => Some(CallFragment {
                id: id.as_deref(),
                name: Some(name.as_str()).filter(|name| !name.is_empty()),
                arguments: Some(&arguments.text),
            }),
            _ => None,
        }
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
        self.insert(index, block.into(), item);
        self.stream_call(index);
        Ok(())
    }

    fn insert(&mut self, index: usize, body: Body, item: Value) {
        self.ended.retain(|(at, _)| *at != index);
        let part = self.next_part;
        self.next_part += 1;
        if let Some(by_index) = &mut self.by_index {
            by_index.insert(part, index);
        }
        let draft = Draft {
            part,
            started: false,
            item,
            body,
            restated: false,
        };
        self.open.insert(index, draft);
    }

    /// A part joins the message and opens to the consumer.
    fn start(&mut self, part: u32, block: Block, name: Option<SmolStr>) {
        self.parts.push(part);
        self.out.push(Event::Start {
            index: part,
            block,
            name,
        });
    }

    /// Append text, reasoning, or a call's argument JSON to the item at
    /// `index`. Owned text moves into the event that carries it.
    pub fn push<'a>(
        &mut self,
        index: usize,
        fragment: impl Into<Cow<'a, str>>,
    ) -> Result<(), Error> {
        self.push_text(index, fragment.into())
    }

    fn push_text(&mut self, index: usize, fragment: Cow<'_, str>) -> Result<(), Error> {
        if fragment.is_empty() {
            return Ok(());
        }
        let draft = self.draft(index)?;
        let block = match &mut draft.body {
            Body::Text(text) => {
                text.push_str(&fragment);
                Block::Text
            }
            Body::Reasoning { text, .. } => {
                text.push_str(&fragment);
                Block::Reasoning
            }
            Body::Call { arguments, .. } => {
                arguments.push(&fragment);
                self.stream_call(index);
                return Ok(());
            }
            Body::Opaque { .. } | Body::Media(_) => {
                return Err(Error::decode(
                    format!("the reply wrote text to the whole item {index}"),
                    &fragment,
                ));
            }
        };
        let part = draft.part;
        if !std::mem::replace(&mut draft.started, true) {
            self.start(part, block, None);
        }
        self.out.push(Event::Delta {
            index: part,
            text: fragment.into_owned(),
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
        let part = self.closed(index).ok_or_else(|| not_open(index))?;
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
            Body::Text(body) | Body::Reasoning { text: body, .. } => {
                if body != text {
                    text.clone_into(body);
                    draft.restated = true;
                }
            }
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

    /// Open and finish the item at `index` in one step.
    pub fn whole<'a>(
        &mut self,
        index: usize,
        block: Open,
        item: Value,
        text: impl Into<Cow<'a, str>>,
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

    /// One fragment of a call the provider streams under wire `index`,
    /// opening the call at its first fragment. Its id and name may arrive in
    /// any fragment, and the first name it is given stays. A new id under
    /// the index starts a new call once the held call's arguments are a
    /// complete object or the new fragment names a tool (some providers send
    /// a fresh id, and no name, with every chunk of one call).
    pub fn fragment(&mut self, index: usize, fragment: CallFragment<'_>) -> Result<(), Error> {
        let new_id = fragment.id.filter(|id| stated(id));
        self.take_over(index, new_id, fragment.name)?;
        if !self.open.contains_key(&index) {
            let body = Body::Call {
                id: None,
                name: SmolStr::default(),
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
            *id = Some(call_id.into());
        }
        if name.is_empty()
            && let Some(fragment) = fragment.name.filter(|name| !name.is_empty())
        {
            *name = fragment.into();
        }
        if let Some(fragment) = fragment.arguments {
            arguments.push(fragment);
        }
        self.stream_call(index);
        Ok(())
    }

    /// When another call takes over wire `index`, the one it held ends.
    fn take_over(
        &mut self,
        index: usize,
        new_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<(), Error> {
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
        Ok(())
    }

    /// Carry what the open call at `index` has not streamed yet: its start
    /// once it names a tool, then the argument text since the last fragment.
    fn stream_call(&mut self, index: usize) {
        let Some(Draft {
            part,
            started,
            body: Body::Call {
                name, arguments, ..
            },
            ..
        }) = self.open.get_mut(&index)
        else {
            return;
        };
        if !*started {
            if name.is_empty() {
                return;
            }
            *started = true;
            self.parts.push(*part);
            self.out.push(Event::Start {
                index: *part,
                block: Block::ToolCall,
                name: Some(name.clone()),
            });
        }
        if let Some(text) = arguments.unstreamed() {
            self.out.push(Event::Delta { index: *part, text });
        }
    }

    /// The id a closing call takes, and the item it keeps: its own, or a
    /// fresh one when the provider sent none or one an earlier call holds.
    /// A renamed call keeps no item, since the item names the id it lost.
    fn distinct_call_id(&mut self, id: Option<SmolStr>, item: Value) -> (SmolStr, Value) {
        let (id, item) = match id {
            Some(id) if !self.call_ids.contains(&id) => (id, item),
            None if self.call_id_slot => (self.local_id(), item),
            Some(_) | None => (self.local_id(), Value::Null),
        };
        self.call_ids.push(id.clone());
        (id, item)
    }

    fn local_id(&mut self) -> SmolStr {
        loop {
            self.ids = self.ids.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let id = smol_str::format_smolstr!("call_{:016x}", self.ids);
            if !self.call_ids.contains(&id) {
                return id;
            }
        }
    }

    /// Emit a part the provider stated whole.
    fn whole_part(&mut self, part: u32, whole: AssistantPart) {
        self.parts.push(part);
        self.out.push(Event::Part {
            index: part,
            part: whole,
        });
    }

    /// Close a call: its arguments parsed, its id made distinct.
    fn close_call(
        &mut self,
        part: u32,
        id: Option<SmolStr>,
        name: &str,
        mut arguments: Arguments,
        item: Value,
    ) {
        if name.is_empty() {
            return;
        }
        let (input, invalid) = arguments.end();
        // A call that streamed no text (none arrived, or only a
        // placeholder `null`) states the arguments it ends with.
        if arguments.streamed == 0 {
            self.out.push(Event::Delta {
                index: part,
                text: serde_json::to_string(&input).unwrap_or_default(),
            });
        }
        let (id, item) = self.distinct_call_id(id, item);
        if !item.is_null() {
            let fp = fingerprint(Canon::Call(name, &input));
            self.items.insert(part, (item, fp));
        }
        self.out.push(Event::End {
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
    }

    fn close_item(&mut self, index: usize, complete: bool) -> Result<(), Error> {
        self.stream_call(index);
        let Draft {
            part,
            started,
            item,
            body,
            restated,
        } = self.open.remove(&index).ok_or_else(|| not_open(index))?;
        self.ended.retain(|(at, _)| *at != index);
        self.ended.push((index, part));
        let item = match (complete, &body) {
            (true, _) | (false, Body::Opaque { .. } | Body::Media(_)) => item,
            (false, _) => {
                self.first_incomplete =
                    Some(self.first_incomplete.map_or(part, |first| first.min(part)));
                Value::Null
            }
        };
        let (block, redacted, text) = match body {
            Body::Text(text) if text.is_empty() && item.is_null() => return Ok(()),
            Body::Text(text) => (Block::Text, false, text),
            Body::Reasoning { text, redacted }
                if text.is_empty() && !redacted && item.is_null() =>
            {
                return Ok(());
            }
            Body::Reasoning { text, redacted } => (Block::Reasoning, redacted, text),
            Body::Opaque { replay } => {
                self.opaque.insert(part, replay && complete);
                self.items.insert(part, (item, String::new()));
                self.whole_part(part, AssistantPart::Opaque(Opaque::default()));
                return Ok(());
            }
            Body::Media(media) => {
                // An image may carry state its producer replays (Gemini's
                // thought signature); nothing else about it can go stale.
                if !item.is_null() {
                    self.items.insert(part, (item, String::new()));
                }
                self.whole_part(part, AssistantPart::Media(media));
                return Ok(());
            }
            Body::Call {
                id,
                name,
                arguments,
            } => {
                self.close_call(part, id, &name, arguments, item);
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
            self.start(part, block, None);
        }
        // A restatement replaces what streamed.
        self.out.push(Event::End {
            index: part,
            redacted,
            text: restated.then_some(text),
            call: None,
        });
        Ok(())
    }

    /// The reply ends: what is still open closes, the parts' provider state
    /// is attached, and the finish follows. A reply that names no reason
    /// fails, as does one that ends with a call left open short of the
    /// output limit.
    pub fn end(&mut self, end: End) -> Result<Flow, Error> {
        let unfinished_call = self
            .open
            .values()
            .any(|draft| matches!(draft.body, Body::Call { .. }));
        // What is still open closes in the order it opened.
        let first_open = |open: &BTreeMap<usize, Draft>| {
            let first = open.iter().min_by_key(|(_, draft)| draft.part);
            first.map(|(index, _)| *index)
        };
        while let Some(index) = first_open(&self.open) {
            self.close_item(index, false)?;
        }
        let Some((reason, raw)) = end.reason else {
            return Err(Error::new(
                Kind::Incomplete,
                "the provider ended the reply without a finish reason",
            ));
        };
        if reason != Reason::Length && unfinished_call {
            return Err(Error::new(
                Kind::Incomplete,
                "the provider ended the reply with a tool call it never finished",
            ));
        }
        if end.response_id.is_some() || end.model.is_some() {
            self.out.push(Event::Metadata {
                id: end.response_id.filter(|id| !id.is_empty()),
                model: end.model.filter(|model| !model.is_empty()),
            });
        }
        let canonical_from = self.first_incomplete.unwrap_or(u32::MAX);
        for (part, (item, fp)) in std::mem::take(&mut self.items) {
            let replay = self.opaque.get(&part).copied();
            if part >= canonical_from && replay.is_none() {
                continue;
            }
            let mut state = crate::json::object([("item".to_owned(), item)]);
            match replay {
                Some(replay) => {
                    state.insert("replay".into(), (replay && part < canonical_from).into())
                }
                None => state.insert("fp".into(), fp.into()),
            };
            let meta = self.meta(state);
            self.out.push(Event::Meta { index: part, meta });
        }
        let reason = match reason {
            Reason::Stop if !self.call_ids.is_empty() => Reason::ToolCalls,
            reason => reason,
        };
        let meta = end
            .raw_usage
            .filter(Value::is_object)
            .map_or_default(|usage| self.meta(crate::json::object([("usage".to_owned(), usage)])));
        let order = self.order();
        self.out.push(Event::Finish {
            finish: Finish {
                reason,
                raw,
                message: end.message,
            },
            usage: end.usage,
            meta,
            order,
        });
        Ok(Flow::Done)
    }

    /// `state` as the meta of this protocol.
    fn meta(&self, state: Map<String, Value>) -> Meta {
        let mut meta = Meta::new();
        meta.insert(self.key.to_owned(), state.into());
        meta
    }

    /// The parts' message order, when it is not the order they opened in.
    fn order(&self) -> Option<Vec<u32>> {
        if self.by_index.is_none() && self.lead.is_none() {
            return None;
        }
        let mut parts = self.parts.clone();
        if let Some(by_index) = &self.by_index {
            // Sorting keys in their natural order compiles to a fraction of
            // a sort by a key function.
            let index = |part: &u32| by_index.get(part).copied().unwrap_or(usize::MAX);
            let mut keyed: Vec<(usize, u32)> =
                parts.iter().map(|part| (index(part), *part)).collect();
            keyed.sort_unstable();
            parts = keyed.into_iter().map(|(_, part)| part).collect();
        }
        if let Some(at) = self
            .lead
            .and_then(|lead| parts.iter().position(|part| *part == lead))
        {
            parts[..=at].rotate_right(1);
        }
        (!parts.is_sorted()).then_some(parts)
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
    for (key, value) in delta.iter().filter(|(key, _)| *key != "type") {
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
