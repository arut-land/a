//! Provider state as the JSON text it was written as. Only the protocol
//! that wrote a key reads it, and only when it encodes a request (once per
//! message, the conversation keeping what it encoded), so the state is
//! parsed there and nowhere else: a transcript holds a string per key, not
//! a tree of values.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use serde_json::value::RawValue;

/// Protocol-keyed provider state on a part or a reply.
///
/// Two are equal when their text is: this crate writes state compact, an
/// object's keys in order, so the same state has the same text.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Meta(BTreeMap<String, Raw>);

#[derive(Clone)]
pub(crate) struct Raw(Box<RawValue>);

impl Raw {
    /// `value`'s text, written once. State this crate writes always
    /// encodes.
    fn new(value: &(impl Serialize + ?Sized)) -> Self {
        Self(serde_json::value::to_raw_value(value).unwrap_or_default())
    }

    /// State read as it was written: the text of a reply this crate logged.
    pub(crate) fn from_text(text: Box<RawValue>) -> Self {
        Self(text)
    }

    fn value(&self) -> Value {
        serde_json::from_str(self.0.get()).unwrap_or_default()
    }
}

impl PartialEq for Raw {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}

impl Eq for Raw {}

impl Serialize for Raw {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

/// Read through a tree: a part's tag makes serde buffer its fields, and a
/// buffer cannot hand on raw text. [`Response::from_json`] reads the text
/// as it is.
///
/// [`Response::from_json`]: crate::Response::from_json
impl<'de> Deserialize<'de> for Raw {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Value::deserialize(d).map(|value| Self::new(&value))
    }
}

impl Meta {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The state under `key`, parsed into a tree, for state a protocol
    /// edits.
    pub fn get(&self, key: &str) -> Option<Value> {
        self.0.get(key).map(Raw::value)
    }

    /// The state under `key` read as `T`, which may borrow from it; `None`
    /// when absent or of another shape.
    pub fn parse<'a, T: Deserialize<'a>>(&'a self, key: &str) -> Option<T> {
        serde_json::from_str(self.raw(key)?).ok()
    }

    /// The state under `key`, as its text.
    pub fn raw(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|raw| raw.0.get())
    }

    pub fn insert(&mut self, key: impl Into<String>, value: &(impl Serialize + ?Sized)) {
        self.0.insert(key.into(), Raw::new(value));
    }

    pub(crate) fn insert_raw(&mut self, key: String, raw: Raw) {
        self.0.insert(key, raw);
    }

    /// `more` over this state, key by key; object values merge one level
    /// down, so state that arrives in pieces (a signature after the text)
    /// accumulates.
    pub(crate) fn merge(&mut self, more: Self) {
        for (key, raw) in more.0 {
            match self.0.get_mut(&key) {
                Some(have) => match (have.value(), raw.value()) {
                    (Value::Object(mut fields), Value::Object(add)) => {
                        fields.extend(add);
                        *have = Raw::new(&fields);
                    }
                    _ => *have = raw,
                },
                None => {
                    self.0.insert(key, raw);
                }
            }
        }
    }
}

impl FromIterator<(String, Value)> for Meta {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(state: I) -> Self {
        let mut meta = Self::new();
        for (key, value) in state {
            meta.insert(key, &value);
        }
        meta
    }
}

impl<const N: usize> From<[(String, Value); N]> for Meta {
    fn from(state: [(String, Value); N]) -> Self {
        state.into_iter().collect()
    }
}

impl fmt::Debug for Meta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raw = self.0.iter().map(|(key, raw)| (key, raw.0.get()));
        f.debug_map().entries(raw).finish()
    }
}
