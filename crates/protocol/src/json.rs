//! Payloads the protocol carries without reading: tool inputs and outputs,
//! model replies, extension state.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use serde_json::value::RawValue;

/// A JSON value kept as the text it was decoded from (or encoded to, once),
/// and written out again as that text: decoding an entry checks its
/// payload but builds nothing of it, and encoding it is a copy. Whoever
/// reads a payload parses it, into its own type or a [`Value`].
///
/// Two are equal when their text is: what this build encodes is compact,
/// with an object's keys in order, so the same value has the same text.
#[derive(Clone)]
pub struct Json(Box<RawValue>);

impl Json {
    /// `value`, encoded as a [`Value`] encodes: its text is what encoding
    /// the tree would have written.
    pub fn new(value: &Value) -> Self {
        Self(serde_json::value::to_raw_value(value).expect("a value always encodes"))
    }

    /// `value` encoded straight to text, with no tree between: what a
    /// model's reply is stored as.
    pub fn encode<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<Self> {
        serde_json::value::to_raw_value(value).map(Self)
    }

    pub fn null() -> Self {
        Self::new(&Value::Null)
    }

    /// The JSON text.
    pub fn raw(&self) -> &str {
        self.0.get()
    }

    /// The payload as a tree; `null` if it nests deeper than a tree may.
    pub fn value(&self) -> Value {
        self.parse().unwrap_or_default()
    }

    /// The payload as a `T`.
    pub fn parse<T: DeserializeOwned>(&self) -> serde_json::Result<T> {
        serde_json::from_slice(self.raw().as_bytes())
    }
}

impl Default for Json {
    fn default() -> Self {
        Self::null()
    }
}

/// As the [`Value`] it converts to, such as a string or a number.
impl<T: Into<Value>> From<T> for Json {
    fn from(value: T) -> Self {
        Self::new(&value.into())
    }
}

impl PartialEq for Json {
    fn eq(&self, other: &Self) -> bool {
        self.raw() == other.raw()
    }
}

impl Eq for Json {}

impl fmt::Debug for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.raw())
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.raw())
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(d).map(Self)
    }
}
