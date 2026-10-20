//! A conversation as requests send it: its messages shared by every request
//! that carries them, growing at their end, and what each protocol encoded
//! of them kept beside them, so a message is encoded once however many
//! requests send it.
//!
//! A protocol encodes a conversation as a left fold ([`Fold`]): each
//! message, as [`Prepare`] passes it on for the target, extends the wire
//! items; the items no later message can change are written as bytes
//! there and then, once, and the rest stay open in the fold. What a
//! conversation keeps per encoding is the fold after its last message and
//! the settled bytes. A request for a conversation that grew since folds
//! only the new messages; one that changed before its end (an edit, an
//! insertion) folds from the start. Either way the bytes are the ones
//! folding from the start makes.

use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

#[cfg(test)]
use serde::{Deserialize, Deserializer};
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::error::Error;
use crate::prepare::Prepare;
use crate::protocol::Target;
use crate::spec::{Message, SmolStr};

/// How many encodings of one conversation it keeps: the target it is sent
/// to, and one more for a request that goes elsewhere meanwhile.
const KEPT: usize = 2;

/// Marks each version of a message: a message pushed, inserted or changed
/// takes a mark no other message ever had.
fn mark() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A conversation's messages. Cloning one shares them; changing a clone
/// copies them first.
#[derive(Clone, Default)]
pub struct Messages(Arc<List>);

#[derive(Clone, Default)]
struct List {
    messages: Vec<Message>,
    /// Each message's mark: an encoding of a prefix ending in a message
    /// with the same mark is still the prefix's.
    marks: Vec<u64>,
    /// The encodings kept, shared with every copy of this list.
    encodings: Arc<Mutex<Vec<Encoding>>>,
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

    /// The last message, to change.
    pub fn last_mut(&mut self) -> Option<&mut Message> {
        let list = self.list();
        *list.marks.last_mut()? = mark();
        list.messages.last_mut()
    }

    /// Whether the first `len` messages are those an encoding of `len`
    /// messages ending in `mark` folded.
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
        Self(Arc::new(List {
            messages,
            marks,
            encodings: Arc::default(),
        }))
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

#[cfg(test)]
impl<'de> Deserialize<'de> for Messages {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::deserialize(deserializer).map(Self::from)
    }
}

/// A protocol's encoding of a conversation, a prepared message at a time.
pub(crate) trait Fold: Clone + Send + 'static {
    /// What else the encoding reads: the quirks and model, and anything
    /// the request says that shapes its messages. An encoding kept for
    /// another one is not this one's.
    type Cx: PartialEq + Send + 'static;

    fn cx(&self) -> &Self::Cx;

    /// Folds message `at` of the prepared conversation; the items no later
    /// message can change go to `out`, in order.
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

/// One encoding kept, beside its fold: the first `len` messages folded,
/// the last of them marked `mark`.
struct Kept {
    provider: SmolStr,
    model: SmolStr,
    len: usize,
    mark: u64,
    pass: Prepare,
    settled: Vec<Shared>,
}

/// An encoding kept and its fold, a [`Fold`] of some protocol.
type Encoding = (Kept, Box<dyn Any + Send>);

#[cfg(test)]
thread_local! {
    /// How many messages this thread's encodings folded.
    pub(crate) static FOLDED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Kept {
    /// The encoding of `messages` kept for `target` whose fold `fits`, or
    /// a new one.
    fn take(
        messages: &Messages,
        target: &Target<'_>,
        fits: &dyn Fn(&(dyn Any + Send)) -> bool,
    ) -> (Self, Option<Box<dyn Any + Send>>) {
        let mut kept = (messages.0.encodings.lock()).unwrap_or_else(PoisonError::into_inner);
        let at = kept.iter().position(|(kept, fold)| {
            kept.provider == target.provider.id
                && kept.model == target.model
                && fits(&**fold)
                && messages.holds(kept.len, kept.mark)
        });
        if let Some(at) = at {
            let (kept, fold) = kept.remove(at);
            return (kept, Some(fold));
        }
        let new = Self {
            provider: target.provider.id.clone(),
            model: target.model.into(),
            len: 0,
            mark: 0,
            pass: Prepare::default(),
            settled: Vec::new(),
        };
        (new, None)
    }

    /// Folds the messages of `messages` past those this encoding folded,
    /// into `fold`; what settles is written to one buffer, shared once.
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

    /// Keeps this encoding, with its `fold`, first among those of
    /// `messages`; what the request goes on with is a copy.
    fn keep(mut self, messages: &Messages, fold: Box<dyn Any + Send>) -> (Prepare, Vec<Shared>) {
        let copy = (self.pass.clone(), self.settled.clone());
        if let Some(&mark) = messages.0.marks.last() {
            (self.len, self.mark) = (messages.len(), mark);
            let mut kept = (messages.0.encodings.lock()).unwrap_or_else(PoisonError::into_inner);
            kept.truncate(KEPT - 1);
            kept.insert(0, (self, fold));
        }
        copy
    }
}

/// The conversation's end, for this request only: what `pass` held goes
/// to `fold`, and the bytes of what then settles join `settled`.
fn end(
    mut pass: Prepare,
    mut settled: Vec<Shared>,
    fold: &mut dyn Step,
) -> Result<Vec<Shared>, Error> {
    let mut bytes = Settled::default();
    pass.finish(fold, &mut bytes)?;
    settled.extend(bytes.shared());
    Ok(settled)
}

/// Items that settled, as JSON separated by commas: each written once,
/// from whatever the protocol holds it as, into the buffer the requests
/// then share.
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

    /// The run written, shared; none when nothing settled.
    fn shared(self) -> Option<Shared> {
        (!self.0.is_empty()).then(|| self.0.into())
    }
}

/// `messages` folded by `fold` for `target`, from what was kept of them
/// when it is still theirs: the fold after the conversation's end, and the
/// items it settled, as bytes.
pub(crate) fn fold<F: Fold>(
    messages: &Messages,
    target: &Target<'_>,
    fold: F,
) -> Result<(F, Vec<Shared>), Error> {
    let fits = |kept: &(dyn Any + Send)| {
        kept.downcast_ref::<F>()
            .is_some_and(|kept| kept.cx() == fold.cx())
    };
    let (mut kept, found) = Kept::take(messages, target, &fits);
    let mut state = match found.map(<Box<dyn Any + Send>>::downcast::<F>) {
        Some(Ok(found)) => *found,
        _ => fold,
    };
    kept.grow(messages, target, &mut state)?;
    let (pass, settled) = kept.keep(messages, Box::new(state.clone()));
    let settled = end(pass, settled, &mut state)?;
    Ok((state, settled))
}

/// An array of a body written from shared bytes: a conversation encoded
/// for one request, or the request's tool definitions. The items its fold
/// makes only for this request (`head` and `tail`) go around the bytes it
/// settled.
#[derive(Debug)]
pub struct History {
    /// The body field the items are.
    pub key: &'static str,
    pub head: Vec<Value>,
    /// Runs of items, separated by commas; none is empty.
    pub settled: Vec<Shared>,
    pub tail: Vec<Value>,
    /// Text around the settled runs, when they are one element of the
    /// array rather than its elements (Gemini's `functionDeclarations`).
    pub wrap: Option<(&'static str, &'static str)>,
}

impl History {
    pub(crate) fn new(key: &'static str, settled: Vec<Shared>, tail: Vec<Value>) -> Self {
        Self {
            key,
            head: Vec::new(),
            settled,
            tail,
            wrap: None,
        }
    }

    /// Writes the items as a JSON array to `out`, the settled bytes as they
    /// are.
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
        for bytes in &self.settled {
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

/// Bytes a kept encoding and the requests that carry it share.
pub(crate) type Shared = Arc<[u8]>;

/// A body being written: what is written now, and the shared bytes it
/// takes as they are.
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

    /// A comma before every element but the first.
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
