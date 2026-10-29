//! Shared conversations cache each protocol's fold and settled bytes.
//! Appends encode only new messages; edits invalidate the affected prefix.
//! Cached and fresh folds must produce identical bytes.

use std::any::Any;
use std::borrow::Cow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Deserializer};
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::error::Error;
use crate::json::encoded_bytes;
use crate::prepare::Prepare;
use crate::protocol::Target;
use crate::spec::{Message, SmolStr};

/// Settled instruction atoms share their storage when a compiler forks.
#[derive(Clone, Default)]
pub(crate) struct SharedList<T>(rpds::VectorSync<Arc<T>>);

impl<T> SharedList<T> {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter().map(Arc::as_ref)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T> Extend<T> for SharedList<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        for item in items {
            self.0.push_back_mut(Arc::new(item));
        }
    }
}

impl<T: Clone> From<SharedList<T>> for Vec<T> {
    fn from(items: SharedList<T>) -> Self {
        items.iter().cloned().collect()
    }
}

/// Keep the current target and one alternate without unbounded cache growth.
const KEPT: usize = 2;

/// Marks each version of a message: a message pushed, inserted or changed
/// takes a mark no other message ever had.
fn mark() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A conversation's messages. Cloning one shares them; changing a clone
/// copies them first.
#[derive(Clone)]
pub struct Messages(Arc<List>, Option<Arc<Mutex<Vec<Encoding>>>>);

impl Default for Messages {
    fn default() -> Self {
        Self(Arc::default(), Some(Arc::default()))
    }
}

#[derive(Clone, Default)]
struct List {
    messages: Vec<Message>,
    /// Each message's mark: an encoding of a prefix ending in a message
    /// with the same mark is still the prefix's.
    marks: Vec<u64>,
}

impl Messages {
    pub fn new() -> Self {
        Self::default()
    }

    fn list(&mut self) -> &mut List {
        Arc::make_mut(&mut self.0)
    }

    pub fn push(&mut self, message: Message) {
        let list = self.list();
        list.messages.push(message);
        list.marks.push(mark());
    }

    pub fn insert(&mut self, at: usize, message: Message) {
        let list = self.list();
        list.messages.insert(at, message);
        list.marks.insert(at, mark());
    }

    pub fn last_mut(&mut self) -> Option<&mut Message> {
        let list = self.list();
        *list.marks.last_mut()? = mark();
        list.messages.last_mut()
    }

    /// The message at `at`, to change: it and every message after it take
    /// new marks, as an encoding of any prefix past `at` holds it.
    pub fn get_mut(&mut self, at: usize) -> Option<&mut Message> {
        let list = self.list();
        for kept in list.marks.get_mut(at..)? {
            *kept = mark();
        }
        list.messages.get_mut(at)
    }

    fn holds(&self, len: usize, mark: u64) -> bool {
        len.checked_sub(1)
            .is_some_and(|last| self.0.marks.get(last) == Some(&mark))
    }
}

impl std::ops::Deref for Messages {
    type Target = [Message];

    fn deref(&self) -> &[Message] {
        &self.0.messages
    }
}

impl From<Vec<Message>> for Messages {
    fn from(messages: Vec<Message>) -> Self {
        let marks = messages.iter().map(|_| mark()).collect();
        Self(Arc::new(List { messages, marks }), Some(Arc::default()))
    }
}

impl FromIterator<Message> for Messages {
    fn from_iter<I: IntoIterator<Item = Message>>(messages: I) -> Self {
        Vec::from_iter(messages).into()
    }
}

impl PartialEq for Messages {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl fmt::Debug for Messages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.messages.fmt(f)
    }
}

impl Serialize for Messages {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.messages.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Messages {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let json = crate::Json::deserialize(deserializer)?;
        Self::from_json(json.raw()).map_err(serde::de::Error::custom)
    }
}

/// A canonical conversation, or a retained encoding with a log-backed source.
#[derive(Clone)]
pub struct Conversation {
    source: Source,
    scope: Option<crate::Json>,
    fitting: Option<crate::fit::Limits>,
}

#[derive(Clone)]
enum Source {
    Canonical(Messages),
    Retained {
        resident: Resident,
        tail: Messages,
        extra: Arc<Vec<Message>>,
        restore: Arc<dyn Fn() -> Result<Messages, Error> + Send + Sync>,
    },
}

/// The active target's continuation. Canonical settled messages belong to the log.
#[derive(Clone, Default)]
pub struct Resident {
    encoding: Arc<Mutex<Option<Encoding>>>,
    offset: usize,
    mark: u64,
    images: usize,
}

/// A frozen compiler checkpoint into shared settled fragments.
#[derive(Clone)]
pub struct Prefix(Arc<Encoding>);

impl Prefix {
    pub fn scope(&self) -> Option<&crate::Json> {
        self.0.0.compiler.scope.as_ref()
    }
}

impl fmt::Debug for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prefix")
            .field("messages", &self.0.0.len)
            .finish()
    }
}

impl From<Prefix> for Resident {
    fn from(prefix: Prefix) -> Self {
        let (kept, _) = &*prefix.0;
        Self {
            encoding: Arc::new(Mutex::new(Some((*prefix.0).clone()))),
            offset: kept.len,
            mark: kept.mark,
            images: kept.images,
        }
    }
}

impl Resident {
    pub fn prefix(&self) -> Option<Prefix> {
        let encoding = self.encoding.lock().unwrap_or_else(PoisonError::into_inner);
        let encoding = encoding.as_ref()?;
        encoding
            .0
            .pass
            .closed()
            .then(|| Prefix(Arc::new(encoding.clone())))
    }

    /// Relinquishes canonical messages covered by the encoded continuation.
    /// Protocol folds retain their unsettled tail; canonical rewrites reload the log.
    pub fn retain(&mut self, tail: &mut Messages) {
        let mut encoding = self.encoding.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((kept, _)) = &*encoding else { return };
        tail.1 = None;
        let Some(count) = kept.len.checked_sub(self.offset) else {
            *encoding = None;
            return;
        };
        if count == 0 {
            if kept.mark != self.mark {
                *encoding = None;
            }
            return;
        }
        if count > tail.len() || !tail.holds(count, kept.mark) {
            *encoding = None;
            return;
        }
        self.offset = kept.len;
        self.mark = kept.mark;
        self.images = kept.images;
        if count == tail.len() && Arc::get_mut(&mut tail.0).is_none() {
            tail.0 = Arc::default();
            return;
        }
        let list = tail.list();
        list.messages.drain(..count);
        list.marks.drain(..count);
    }

    /// Whether a prefix has left canonical memory.
    pub const fn has_prefix(&self) -> bool {
        self.offset != 0
    }
}

impl Conversation {
    /// A source may borrow its producer's generation. Materialize canonical data
    /// before retaining it beyond that generation's lifetime. `restore` reconstructs
    /// exactly the canonical history represented by `resident` and `tail`.
    pub fn retained(
        resident: Resident,
        mut tail: Messages,
        restore: impl Fn() -> Result<Messages, Error> + Send + Sync + 'static,
    ) -> Self {
        // Canonical exports own their caches; they cannot attach bytes to this decoded tail.
        tail.1 = None;
        Self {
            source: Source::Retained {
                resident,
                extra: Arc::default(),
                tail,
                restore: Arc::new(restore),
            },
            scope: None,
            fitting: None,
        }
    }

    /// Names the immutable instruction/offer generation and projection semantics.
    #[must_use]
    pub fn with_scope(mut self, scope: crate::Json) -> Self {
        self.scope = Some(scope);
        self
    }

    pub(crate) fn fitting(&mut self, limits: crate::fit::Limits) {
        self.fitting = Some(limits);
    }

    /// Freezes a closed encoded frontier; later parent appends cannot change it.
    pub fn prefix(&self) -> Option<Prefix> {
        match &self.source {
            Source::Retained { resident, tail, .. } => {
                let prefix = resident.prefix()?;
                let kept = &prefix.0.0;
                let stop = resident.offset + tail.len();
                (kept.len == stop
                    && kept.compiler.scope == self.scope
                    && if tail.is_empty() {
                        kept.mark == resident.mark
                    } else {
                        tail.holds(tail.len(), kept.mark)
                    })
                .then_some(prefix)
            }
            Source::Canonical(_) => None,
        }
    }

    /// Loads canonical history only for consumers that need it.
    pub fn canonical(&self) -> Result<Cow<'_, Messages>, Error> {
        match &self.source {
            Source::Canonical(messages) => Ok(Cow::Borrowed(messages)),
            Source::Retained {
                resident,
                tail,
                extra,
                restore,
            } => {
                if resident.offset == 0 {
                    let mut messages = Messages(Arc::clone(&tail.0), Some(Arc::default()));
                    for message in extra.iter() {
                        messages.push(message.clone());
                    }
                    Ok(Cow::Owned(messages))
                } else {
                    restored(restore, extra).map(Cow::Owned)
                }
            }
        }
    }

    /// Reconstructs a canonical value before a request-specific edit.
    pub fn canonical_mut(&mut self) -> Result<&mut Messages, Error> {
        if matches!(self.source, Source::Retained { .. }) {
            self.source = Source::Canonical(self.canonical()?.into_owned());
        }
        let Source::Canonical(messages) = &mut self.source else {
            unreachable!("canonical conversation")
        };
        Ok(messages)
    }

    /// Appends request-local instructions without reconstructing the prefix.
    pub fn push(&mut self, message: Message) {
        match &mut self.source {
            Source::Canonical(messages) => messages.push(message),
            Source::Retained { extra, .. } => Arc::make_mut(extra).push(message),
        }
    }

    pub fn images(&self) -> usize {
        match &self.source {
            Source::Canonical(messages) => messages.iter().map(images).sum(),
            Source::Retained {
                resident,
                tail,
                extra,
                ..
            } => resident.images + tail.iter().chain(extra.iter()).map(images).sum::<usize>(),
        }
    }
}

impl Default for Conversation {
    fn default() -> Self {
        Messages::new().into()
    }
}

impl From<Messages> for Conversation {
    fn from(messages: Messages) -> Self {
        Self {
            source: Source::Canonical(messages),
            scope: None,
            fitting: None,
        }
    }
}

impl From<Vec<Message>> for Conversation {
    fn from(messages: Vec<Message>) -> Self {
        Messages::from(messages).into()
    }
}

impl FromIterator<Message> for Conversation {
    fn from_iter<I: IntoIterator<Item = Message>>(messages: I) -> Self {
        Messages::from_iter(messages).into()
    }
}

impl fmt::Debug for Conversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Source::Canonical(messages) => f
                .debug_struct("Conversation")
                .field("messages", messages)
                .field("scope", &self.scope)
                .field("fitting", &self.fitting)
                .finish(),
            Source::Retained {
                resident,
                tail,
                extra,
                ..
            } => f
                .debug_struct("Retained")
                .field("scope", &self.scope)
                .field("fitting", &self.fitting)
                .field("prefix", &resident.offset)
                .field("tail", tail)
                .field("extra", extra)
                .finish(),
        }
    }
}

impl PartialEq for Conversation {
    fn eq(&self, other: &Self) -> bool {
        match (&self.source, &other.source) {
            (Source::Canonical(a), Source::Canonical(b)) => a == b,
            (
                Source::Retained {
                    resident: a,
                    tail: at,
                    extra: ae,
                    ..
                },
                Source::Retained {
                    resident: b,
                    tail: bt,
                    extra: be,
                    ..
                },
            ) => {
                Arc::ptr_eq(&a.encoding, &b.encoding)
                    && a.offset == b.offset
                    && a.mark == b.mark
                    && at == bt
                    && ae == be
            }
            (Source::Canonical(_), Source::Retained { .. })
            | (Source::Retained { .. }, Source::Canonical(_)) => false,
        }
    }
}

impl Serialize for Conversation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.canonical()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Conversation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Messages::deserialize(deserializer).map(Into::into)
    }
}

fn images(message: &Message) -> usize {
    let parts = |parts: &[crate::UserPart]| {
        parts.iter().filter(|part| {
        matches!(part, crate::UserPart::Media(media) if media.media_type.starts_with("image/"))
    }).count()
    };
    match message {
        Message::User(user) => parts(&user.content),
        Message::Tool(tool) => tool
            .results
            .iter()
            .map(|result| match &result.output {
                crate::Output::Content(content) => parts(content),
                crate::Output::Text(_) | crate::Output::Json(_) => 0,
            })
            .sum(),
        Message::System(_) | Message::Assistant(_) => 0,
    }
}

/// Source-owned bounds apply to requests, never to durable exchanges or outcomes.
#[derive(Clone, Copy, Debug)]
pub struct ContextPolicy {
    pub exchanges: u16,
    pub bytes: u32,
}

impl Serialize for ContextPolicy {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (self.exchanges, self.bytes).serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for ContextPolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (exchanges, bytes) = Deserialize::deserialize(deserializer)?;
        Ok(Self { exchanges, bytes })
    }
}

impl ContextPolicy {
    pub fn assemble(
        self,
        history: &[Message],
        owner: &[Message],
        current: &Messages,
    ) -> Result<Messages, String> {
        let mut used = crate::Json::encode(current)
            .map_err(|e| e.to_string())?
            .raw()
            .len();
        if used > self.bytes as usize {
            return Err("model-only instructions exceed the context ceiling".into());
        }
        let mut select = |messages: &[Message]| -> Result<Vec<Message>, String> {
            let mut selected = Vec::new();
            for exchange in exchanges(messages).rev().take(self.exchanges.into()) {
                let bytes = crate::Json::encode(exchange)
                    .map_err(|e| e.to_string())?
                    .raw()
                    .len();
                if used + bytes > self.bytes as usize {
                    break;
                }
                used += bytes;
                selected.push(exchange);
            }
            Ok(selected.into_iter().rev().flatten().cloned().collect())
        };
        // Owner context has priority over older thread exchanges.
        let context = select(owner)?;
        let history = select(history)?;
        Ok(history
            .into_iter()
            .chain(context)
            .chain(current.iter().cloned())
            .collect())
    }
}

impl ContextPolicy {
    /// Covers every exchange in order, repeating `current` on each input.
    /// Large exchanges become consecutive serialized text fragments. Media
    /// and opaque provider items stay native and indivisible, even when
    /// they exceed the byte target.
    pub fn cover<'a>(
        self,
        prefix: &'a [Message],
        current: &'a Messages,
    ) -> impl Iterator<Item = Result<Messages, String>> + 'a {
        let mut exchanges = exchanges(prefix).peekable();
        let mut fragment: Option<(crate::Json, usize)> = None;
        let mut native = Vec::new().into_iter();
        std::iter::from_fn(move || {
            let result: Result<Option<Messages>, String> = (|| {
                let mandatory =
                    encoded_bytes(current, self.bytes as usize).map_err(|e| e.to_string())?;
                let room = (self.bytes as usize)
                    .checked_sub(mandatory)
                    .ok_or("summary instructions leave no room for context")?;
                if fragment.is_none() && native.len() == 0 {
                    let Some(exchange) = exchanges.next() else {
                        return Ok(None);
                    };
                    if let Ok(bytes) = encoded_bytes(&exchange, room) {
                        let mut messages = exchange.to_vec();
                        let mut used = bytes;
                        for _ in 1..self.exchanges {
                            let Some(next) = exchanges.peek() else { break };
                            let Ok(bytes) = encoded_bytes(next, room - used) else {
                                break;
                            };
                            used += bytes;
                            messages.extend_from_slice(exchanges.next().expect("peeked exchange"));
                        }
                        return Ok(Some(
                            messages
                                .into_iter()
                                .chain(current.iter().cloned())
                                .collect(),
                        ));
                    }
                    let mut parts = Vec::new();
                    let mut exchange = exchange.to_vec();
                    for message in &mut exchange {
                        readable_exchange(message, &mut parts);
                    }
                    fragment = Some((
                        crate::Json::encode(&exchange).map_err(|e| e.to_string())?,
                        0,
                    ));
                    native = parts.into_iter();
                }
                if let Some(message) = native.next() {
                    return Ok(Some(
                        std::iter::once(message)
                            .chain(current.iter().cloned())
                            .collect(),
                    ));
                }
                let Some((json, offset)) = &mut fragment else {
                    return Ok(None);
                };
                let text = json.raw();
                let mut messages: Messages = [
                    Message::system("The next message is a consecutive fragment of a serialized conversation exchange. Summarise its facts, including incomplete text."),
                    Message::user(""),
                ].into_iter().chain(current.iter().cloned()).collect();
                let framing =
                    encoded_bytes(&messages, self.bytes as usize).map_err(|e| e.to_string())?;
                let mut remaining = (self.bytes as usize).saturating_sub(framing);
                let start = *offset;
                for ch in text[start..].chars() {
                    let bytes = match ch {
                        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
                        '\0'..='\u{1f}' => 6,
                        _ => ch.len_utf8(),
                    };
                    if bytes > remaining {
                        break;
                    }
                    remaining -= bytes;
                    *offset += ch.len_utf8();
                }
                if start == *offset {
                    return Err("summary instructions leave no room for a text fragment".into());
                }
                *messages.get_mut(1).expect("fragment message") =
                    Message::user(&text[start..*offset]);
                if *offset == text.len() {
                    fragment = None;
                }
                Ok(Some(messages))
            })();
            result.transpose()
        })
    }
}

// A text fragment must not expose provider-private replay metadata (signatures,
// encrypted reasoning, native ids). Native requests still carry it unchanged.
fn readable_exchange(message: &mut Message, native: &mut Vec<Message>) {
    fn user_parts(parts: &mut [crate::UserPart], native: &mut Vec<Message>) {
        for part in parts {
            match part {
                crate::UserPart::Text(text) => text.meta.clear(),
                crate::UserPart::Media(_) => {
                    native.push(Message::User(crate::User {
                        content: vec![std::mem::replace(
                            part,
                            crate::UserPart::Text(crate::Text::new("[media supplied separately]")),
                        )],
                    }));
                }
            }
        }
    }
    match message {
        Message::System(system) => {
            for text in &mut system.content {
                text.meta.clear();
            }
        }
        Message::User(user) => user_parts(&mut user.content, native),
        Message::Assistant(reply) => {
            let origin = reply.origin.take();
            let mut opaque = Vec::new();
            for part in &mut reply.content {
                match part {
                    crate::AssistantPart::Opaque(_) => {
                        opaque.push(std::mem::replace(
                            part,
                            crate::AssistantPart::Text(crate::Text::new(
                                "[provider item supplied separately]",
                            )),
                        ));
                    }
                    crate::AssistantPart::Media(image) => {
                        native.push(Message::User(crate::User {
                            content: vec![crate::UserPart::Media(image.clone())],
                        }));
                        *part = crate::AssistantPart::Text(crate::Text::new(
                            "[media supplied separately]",
                        ));
                    }
                    crate::AssistantPart::ToolResult(result) => {
                        if let crate::Output::Content(parts) = &mut result.output {
                            user_parts(parts, native);
                        }
                    }
                    crate::AssistantPart::Text(_)
                    | crate::AssistantPart::Reasoning(_)
                    | crate::AssistantPart::ToolCall(_) => {}
                }
                part.meta_mut().clear();
            }
            if !opaque.is_empty() {
                native.push(Message::Assistant(crate::Assistant {
                    content: opaque,
                    origin,
                }));
            }
        }
        Message::Tool(tool) => {
            for result in &mut tool.results {
                result.meta.clear();
                if let crate::Output::Content(parts) = &mut result.output {
                    user_parts(parts, native);
                }
            }
        }
    }
}

fn exchanges(messages: &[Message]) -> impl DoubleEndedIterator<Item = &[Message]> {
    messages.chunk_by(|previous, next| match (previous, next) {
        (Message::System(_), _) => true,
        (Message::Assistant(reply), _) if reply.tool_calls().next().is_none() => false,
        (_, Message::System(_) | Message::User(_)) => false,
        _ => true,
    })
}

/// A protocol's encoding of a conversation, a prepared message at a time.
pub(crate) trait Fold: Clone + Send + Sync + 'static {
    /// Encodings are reusable only with the same request context.
    type Cx: PartialEq + Send + Sync + 'static;

    fn cx(&self) -> &Self::Cx;

    /// Only items no later message can change may settle into `out`.
    fn push(&mut self, at: usize, message: &Message, out: &mut Settled) -> Result<(), Error>;
}

/// A fold's step, past its type: the pass and the code that keeps
/// encodings drive every protocol's fold through one copy.
pub(crate) trait Step {
    fn push(&mut self, at: usize, message: &Message, out: &mut Settled) -> Result<(), Error>;
}

impl<F: Fold> Step for F {
    fn push(&mut self, at: usize, message: &Message, out: &mut Settled) -> Result<(), Error> {
        Fold::push(self, at, message, out)
    }
}

#[derive(Clone, PartialEq)]
struct Compiler {
    provider: SmolStr,
    model: SmolStr,
    protocol: crate::ProtocolKey,
    scope: Option<crate::Json>,
    offers: crate::Tools,
    options: Arc<crate::Options>,
    body: Option<Arc<serde_json::Map<String, Value>>>,
    fitting: crate::fit::Limits,
}

impl Compiler {
    fn of(request: &crate::Request, target: &Target<'_>) -> Self {
        Self {
            provider: target.provider.id.clone(),
            model: target.model.into(),
            protocol: target.provider.protocol.key(),
            scope: request.messages.scope.clone(),
            offers: request.tools.clone(),
            options: Arc::new(request.options.clone()),
            body: request.body.clone().map(Arc::new),
            fitting: request.messages.fitting.unwrap_or_else(|| {
                crate::fit::Limits::of_protocol(target.provider.protocol.key(), target.model)
            }),
        }
    }
}

#[derive(Clone)]
struct Kept {
    compiler: Compiler,
    len: usize,
    mark: u64,
    images: usize,
    pass: Prepare,
    settled: Fragments,
}

type Encoding = (Kept, Arc<dyn Any + Send + Sync>);

#[cfg(test)]
thread_local! {
    /// How many messages this thread's encodings folded.
    pub(crate) static FOLDED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Kept {
    fn take(
        messages: &Messages,
        compiler: Compiler,
        fits: &dyn Fn(&(dyn Any + Send + Sync)) -> bool,
    ) -> (Self, Option<Arc<dyn Any + Send + Sync>>) {
        if let Some(encodings) = &messages.1 {
            let mut kept = encodings.lock().unwrap_or_else(PoisonError::into_inner);
            let at = kept.iter().position(|(kept, fold)| {
                kept.compiler == compiler && fits(&**fold) && messages.holds(kept.len, kept.mark)
            });
            if let Some(at) = at {
                let (kept, fold) = kept.remove(at);
                return (kept, Some(fold));
            }
        }
        let new = Self {
            compiler,
            len: 0,
            mark: 0,
            images: 0,
            pass: Prepare::default(),
            settled: Fragments::default(),
        };
        (new, None)
    }

    fn grow(
        &mut self,
        messages: &Messages,
        target: &Target<'_>,
        fold: &mut dyn Step,
    ) -> Result<(), Error> {
        let mut bytes = Settled::default();
        for message in &messages[self.len..] {
            #[cfg(test)]
            FOLDED.with(|folded| folded.set(folded.get() + 1));
            self.pass.push(target, message, fold, &mut bytes)?;
        }
        self.settled.extend(bytes.shared());
        Ok(())
    }

    fn keep(
        mut self,
        messages: &Messages,
        fold: Arc<dyn Any + Send + Sync>,
    ) -> (Prepare, Fragments) {
        let copy = (self.pass.clone(), self.settled.clone());
        if let Some(&mark) = messages.0.marks.last()
            && let Some(encodings) = &messages.1
        {
            (self.len, self.mark) = (messages.len(), mark);
            let mut kept = encodings.lock().unwrap_or_else(PoisonError::into_inner);
            kept.truncate(KEPT - 1);
            kept.insert(0, (self, fold));
        }
        copy
    }
}

/// Sharing settled JSON avoids reserializing it on subsequent requests.
#[derive(Default)]
pub(crate) struct Settled(Vec<u8>);

impl Settled {
    pub(crate) fn item(&mut self, item: &(impl Serialize + ?Sized)) {
        if !self.0.is_empty() {
            self.0.push(b',');
        }
        // Writing to memory cannot fail.
        let _ = serde_json::to_writer(&mut self.0, item);
    }

    fn shared(self) -> Option<Shared> {
        (!self.0.is_empty()).then(|| self.0.into())
    }
}

pub(crate) fn fold<F: Fold>(
    request: &crate::Request,
    target: &Target<'_>,
    initial: F,
) -> Result<(F, Fragments), Error> {
    let compiler = Compiler::of(request, target);
    match &request.messages.source {
        Source::Canonical(messages) => fold_messages(messages, target, initial, compiler),
        Source::Retained {
            resident,
            tail,
            extra,
            restore,
        } => fold_retained(resident, tail, extra, restore, target, initial, compiler),
    }
}

fn fold_messages<F: Fold>(
    messages: &Messages,
    target: &Target<'_>,
    fold: F,
    compiler: Compiler,
) -> Result<(F, Fragments), Error> {
    let fits = |kept: &(dyn Any + Send + Sync)| {
        kept.downcast_ref::<F>()
            .is_some_and(|kept| kept.cx() == fold.cx())
    };
    let (mut kept, found) = Kept::take(messages, compiler, &fits);
    let mut state = found
        .and_then(|found| found.downcast_ref::<F>().cloned())
        .unwrap_or(fold);
    kept.grow(messages, target, &mut state)?;
    let (mut pass, mut settled) = kept.keep(messages, Arc::new(state.clone()));
    let mut bytes = Settled::default();
    pass.finish(&mut state, &mut bytes)?;
    settled.extend(bytes.shared());
    Ok((state, settled))
}

fn restored(
    restore: &Arc<dyn Fn() -> Result<Messages, Error> + Send + Sync>,
    extra: &[Message],
) -> Result<Messages, Error> {
    let mut messages = restore()?;
    for message in extra {
        messages.push(message.clone());
    }
    Ok(messages)
}

fn fold_retained<F: Fold>(
    resident: &Resident,
    tail: &Messages,
    extra: &[Message],
    restore: &Arc<dyn Fn() -> Result<Messages, Error> + Send + Sync>,
    target: &Target<'_>,
    initial: F,
    compiler: Compiler,
) -> Result<(F, Fragments), Error> {
    let stop = resident.offset + tail.len();
    let found = resident
        .encoding
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let found = found.filter(|(kept, fold)| {
        kept.compiler == compiler
            && kept.len >= resident.offset
            && kept.len <= stop
            && (if kept.len == resident.offset {
                kept.mark == resident.mark
            } else {
                tail.holds(kept.len - resident.offset, kept.mark)
            })
            && fold
                .downcast_ref::<F>()
                .is_some_and(|state| state.cx() == initial.cx())
    });
    let canonical;
    let (mut kept, mut state, append) = if let Some((kept, fold)) = found {
        let at = kept.len - resident.offset;
        (
            kept,
            fold.downcast_ref::<F>().expect("matched fold").clone(),
            &tail[at..],
        )
    } else {
        canonical = if resident.offset == 0 {
            Cow::Borrowed(tail)
        } else {
            Cow::Owned(restore()?)
        };
        (
            Kept {
                compiler,
                len: 0,
                mark: 0,
                images: 0,
                pass: Prepare::default(),
                settled: Fragments::default(),
            },
            initial,
            &canonical[..],
        )
    };
    let count = stop - kept.len;
    let mut bytes = Settled::default();
    for message in &append[..count] {
        #[cfg(test)]
        FOLDED.with(|folded| folded.set(folded.get() + 1));
        kept.pass.push(target, message, &mut state, &mut bytes)?;
        kept.images += images(message);
    }
    kept.len = stop;
    kept.mark = if stop > resident.offset {
        tail.0.marks[stop - resident.offset - 1]
    } else {
        resident.mark
    };
    kept.settled.extend(bytes.shared());
    let mut pass = kept.pass.clone();
    let mut settled = kept.settled.clone();
    *resident
        .encoding
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some((kept, Arc::new(state.clone())));
    let mut bytes = Settled::default();
    for message in extra {
        #[cfg(test)]
        FOLDED.with(|folded| folded.set(folded.get() + 1));
        pass.push(target, message, &mut state, &mut bytes)?;
    }
    pass.finish(&mut state, &mut bytes)?;
    settled.extend(bytes.shared());
    Ok((state, settled))
}

/// Immutable settled runs; a branch shares the existing chunk tree.
#[derive(Clone, Default)]
pub struct Fragments(rpds::VectorSync<Shared>);

impl Fragments {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Shared> {
        self.0.iter()
    }
}

impl Extend<Shared> for Fragments {
    fn extend<T: IntoIterator<Item = Shared>>(&mut self, runs: T) {
        for bytes in runs {
            self.0.push_back_mut(bytes);
        }
    }
}

impl From<Vec<Shared>> for Fragments {
    fn from(runs: Vec<Shared>) -> Self {
        let mut fragments = Self::default();
        fragments.extend(runs);
        fragments
    }
}

impl fmt::Debug for Fragments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Request-specific items surround shared, previously settled JSON.
#[derive(Debug)]
pub struct History {
    pub key: &'static str,
    pub head: Vec<Value>,
    /// Runs of items, separated by commas; none is empty.
    pub settled: Fragments,
    pub tail: Vec<Value>,
    /// Text around the settled runs, when they are one element of the
    /// array rather than its elements (Gemini's `functionDeclarations`).
    pub wrap: Option<(&'static str, &'static str)>,
}

impl History {
    pub(crate) fn new(key: &'static str, settled: impl Into<Fragments>, tail: Vec<Value>) -> Self {
        Self {
            key,
            head: Vec::new(),
            settled: settled.into(),
            tail,
            wrap: None,
        }
    }

    pub(crate) fn write(&self, out: &mut Pieces) {
        out.byte(b'[');
        let mut first = true;
        for item in &self.head {
            out.comma(&mut first);
            out.json(item);
        }
        let wrap = self.wrap.filter(|_| !self.settled.is_empty());
        if let Some((open, _)) = wrap {
            out.comma(&mut first);
            out.text(open);
            first = true;
        }
        for bytes in self.settled.iter() {
            out.comma(&mut first);
            out.shared(Arc::clone(bytes));
        }
        if let Some((_, close)) = wrap {
            out.text(close);
            first = false;
        }
        for item in &self.tail {
            out.comma(&mut first);
            out.json(item);
        }
        out.byte(b']');
    }
}

pub(crate) type Shared = Arc<[u8]>;

#[derive(Default)]
pub(crate) struct Pieces {
    done: Vec<Shared>,
    buf: Vec<u8>,
}

impl Pieces {
    pub(crate) fn byte(&mut self, byte: u8) {
        self.buf.push(byte);
    }

    pub(crate) fn text(&mut self, text: &str) {
        self.buf.extend_from_slice(text.as_bytes());
    }

    pub(crate) fn comma(&mut self, first: &mut bool) {
        if !std::mem::take(first) {
            self.byte(b',');
        }
    }

    pub(crate) fn json(&mut self, value: &(impl Serialize + ?Sized)) {
        // Writing to memory cannot fail.
        let _ = serde_json::to_writer(&mut self.buf, value);
    }

    pub(crate) fn shared(&mut self, bytes: Shared) {
        if !self.buf.is_empty() {
            self.done.push(std::mem::take(&mut self.buf).into());
        }
        self.done.push(bytes);
    }

    pub(crate) fn finish(mut self) -> Vec<Shared> {
        if !self.buf.is_empty() {
            self.done.push(self.buf.into());
        }
        self.done
    }
}

#[cfg(test)]
mod tests;
