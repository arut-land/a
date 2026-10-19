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
struct Raw(Box<RawValue>);

impl Raw {
    /// `value`'s text, in a buffer of its size: state can hold a whole
    /// reasoning text, which a growing buffer would hold twice over.
    fn new(value: &Value) -> Self {
        struct Count(usize);

        impl std::io::Write for Count {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 += bytes.len();
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut count = Count(0);
        // A tree always encodes.
        let _ = serde_json::to_writer(&mut count, value);
        let mut text = Vec::with_capacity(count.0);
        let _ = serde_json::to_writer(&mut text, value);
        let text = String::from_utf8(text).unwrap_or_default();
        Self(RawValue::from_string(text).unwrap_or_default())
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
/// buffer cannot hand on raw text.
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

    /// The state under `key`, parsed.
    pub fn get(&self, key: &str) -> Option<Value> {
        self.0.get(key).map(Raw::value)
    }

    /// Field `field` of the state under `key`.
    pub fn field(&self, key: &str, field: &str) -> Option<Value> {
        self.get(key)?.get_mut(field).map(Value::take)
    }

    /// The state under `key`, as its text.
    pub fn raw(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|raw| raw.0.get())
    }

    pub fn insert(&mut self, key: impl Into<String>, value: &Value) {
        self.0.insert(key.into(), Raw::new(value));
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
                        *have = Raw::new(&Value::Object(fields));
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
