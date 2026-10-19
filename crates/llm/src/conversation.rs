//! A conversation as requests send it: its messages shared by every request
//! that carries them, growing at their end, and what each protocol encoded
//! of them kept beside them, so a message is encoded once however many
//! requests send it.
//!
//! A protocol encodes a conversation as a left fold ([`Fold`]): each
//! message, as [`Prepare`] passes it on for the target, extends the wire
//! items; the items no later message can change settle as bytes, and the
//! rest stay open in the fold. What a conversation keeps per encoding is
//! the fold after its last message and the settled bytes. A request for a
//! conversation that grew since folds only the new messages; one that
//! changed before its end (an edit, an insertion) folds from the start.
//! Either way the bytes are the ones folding from the start makes.

use std::any::Any;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
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
    encodings: Arc<Mutex<Vec<Box<dyn Any + Send>>>>,
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

    /// Folds message `at` of the prepared conversation.
    fn push(&mut self, at: usize, message: &Message) -> Result<(), Error>;

    /// Moves the items no later message can change to `out`, in order.
    fn settle(&mut self, out: &mut Vec<Value>);
}

/// One encoding kept: `fold` after the first `len` messages, the last of
/// them marked `mark`.
struct Kept<F: Fold> {
    provider: SmolStr,
    model: SmolStr,
    len: usize,
    mark: u64,
    pass: Prepare,
    fold: F,
    settled: Vec<Bytes>,
}

/// Writes the items `fold` settled to `bytes`, each followed by a comma.
fn settle<F: Fold>(fold: &mut F, items: &mut Vec<Value>, bytes: &mut Vec<u8>) {
    fold.settle(items);
    for item in items.drain(..) {
        // Writing a value to memory cannot fail.
        let _ = serde_json::to_writer(&mut *bytes, &item);
        bytes.push(b',');
    }
}

#[cfg(test)]
thread_local! {
    /// How many messages this thread's encodings folded.
    pub(crate) static FOLDED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `messages` folded by `fold` for `target`, from what was kept of them
/// when it is still theirs: the fold after the conversation's end, and the
/// items it settled, as bytes.
pub(crate) fn fold<F: Fold>(
    messages: &Messages,
    target: &Target<'_>,
    fold: F,
) -> Result<(F, Vec<Bytes>), Error> {
    let provider = &target.provider.id;
    let mine = |kept: &Kept<F>| {
        kept.provider == *provider && kept.model == target.model && kept.fold.cx() == fold.cx()
    };
    let encodings = &messages.0.encodings;
    let found = {
        let mut kept = encodings.lock().unwrap_or_else(PoisonError::into_inner);
        let at = kept.iter().position(|kept| {
            kept.downcast_ref::<Kept<F>>()
                .is_some_and(|kept| mine(kept) && messages.holds(kept.len, kept.mark))
        });
        at.map(|at| kept.remove(at))
    };
    let (mut kept, from) = match found.and_then(|kept| kept.downcast::<Kept<F>>().ok()) {
        Some(kept) => {
            let from = kept.len;
            (*kept, from)
        }
        None => (
            Kept {
                provider: provider.clone(),
                model: target.model.into(),
                len: 0,
                mark: 0,
                pass: Prepare::default(),
                fold,
                settled: Vec::new(),
            },
            0,
        ),
    };
    // What this request settles is written to one buffer.
    let (mut items, mut bytes) = (Vec::new(), Vec::new());
    for message in &messages[from..] {
        #[cfg(test)]
        FOLDED.with(|folded| folded.set(folded.get() + 1));
        kept.pass.push(target, message, &mut kept.fold)?;
        settle(&mut kept.fold, &mut items, &mut bytes);
    }
    if !bytes.is_empty() {
        kept.settled.push(std::mem::take(&mut bytes).into());
    }
    let (mut pass, mut fold, mut settled) =
        (kept.pass.clone(), kept.fold.clone(), kept.settled.clone());
    if let Some(&mark) = messages.0.marks.last() {
        kept.len = messages.len();
        kept.mark = mark;
        let mut kept_all = encodings.lock().unwrap_or_else(PoisonError::into_inner);
        kept_all.truncate(KEPT - 1);
        kept_all.insert(0, Box::new(kept));
    }
    // What the conversation's end implies goes with this request only.
    pass.finish(&mut fold)?;
    settle(&mut fold, &mut items, &mut bytes);
    if !bytes.is_empty() {
        settled.push(bytes.into());
    }
    Ok((fold, settled))
}

/// A conversation encoded for one request: the items its fold makes only
/// for this request (`head` and `tail`) around the bytes it settled.
#[derive(Debug)]
pub struct History {
    /// The body field the items are.
    pub key: &'static str,
    pub head: Vec<Value>,
    /// Items, each followed by a comma.
    pub settled: Vec<Bytes>,
    pub tail: Vec<Value>,
}

impl History {
    /// Writes the items as a JSON array to `out`, the settled bytes as they
    /// are.
    pub(crate) fn write(&self, out: &mut Pieces) {
        out.byte(b'[');
        for (n, item) in self.head.iter().enumerate() {
            if n > 0 {
                out.byte(b',');
            }
            out.json(item);
        }
        let mut first = self.head.is_empty();
        let settled: Vec<&Bytes> = self.settled.iter().filter(|b| !b.is_empty()).collect();
        if let Some((last, rest)) = settled.split_last() {
            if !std::mem::take(&mut first) {
                out.byte(b',');
            }
            for bytes in rest {
                out.bytes((*bytes).clone());
            }
            // The last item's comma stays only to lead the tail.
            out.bytes(if self.tail.is_empty() {
                last.slice(..last.len() - 1)
            } else {
                (*last).clone()
            });
            first = true;
        }
        for item in &self.tail {
            if !std::mem::take(&mut first) {
                out.byte(b',');
            }
            out.json(item);
        }
        out.byte(b']');
    }
}

/// A body being written: what is written now, and the shared bytes it
/// takes as they are.
#[derive(Default)]
pub(crate) struct Pieces {
    done: Vec<Bytes>,
    buf: Vec<u8>,
}

impl Pieces {
    pub(crate) fn byte(&mut self, byte: u8) {
        self.buf.push(byte);
    }

    pub(crate) fn json(&mut self, value: &(impl Serialize + ?Sized)) {
        // Writing to memory cannot fail.
        let _ = serde_json::to_writer(&mut self.buf, value);
    }

    pub(crate) fn bytes(&mut self, bytes: Bytes) {
        if !self.buf.is_empty() {
            self.done.push(std::mem::take(&mut self.buf).into());
        }
        self.done.push(bytes);
    }

    pub(crate) fn finish(mut self) -> Vec<Bytes> {
        if !self.buf.is_empty() {
            self.done.push(self.buf.into());
        }
        self.done
    }
}

#[cfg(test)]
mod tests;
