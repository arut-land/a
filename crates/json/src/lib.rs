//! Payloads carried without being read: a model's tool call arguments, a
//! tool's output, a model's reply as the log keeps it, extension state. The
//! same text goes from the model's reply to the log, the tool and the next
//! request: whoever reads a payload parses it into its own type, and nobody
//! re-encodes one to pass it on.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// A JSON value kept as the text it was decoded from (or encoded to, once),
/// and written out again as that text: decoding a payload checks it but
/// builds nothing of it, and encoding it is a copy.
///
/// Two are equal when their text is: what this workspace encodes is
/// compact, with an object's keys in order, so the same value has the same
/// text. Shared: a clone is a count.
#[derive(Clone)]
pub struct Json(Arc<RawValue>);

impl Json {
    /// `value` encoded straight to text, with no tree between.
    pub fn encode<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<Self> {
        serde_json::value::to_raw_value(value).map(Self::from)
    }

    pub fn null() -> Self {
        Self::from(RawValue::NULL.to_owned())
    }

    /// The JSON text.
    pub fn raw(&self) -> &str {
        self.0.get()
    }

    /// The payload as a `T`, read straight from the text: one that
    /// borrows (a `&str`, a `&RawValue`) borrows it.
    pub fn parse<'a, T: Deserialize<'a>>(&'a self) -> serde_json::Result<T> {
        // From bytes, as every other decoder reads: one reader's code.
        serde_json::from_slice(self.raw().as_bytes())
    }

    /// A payload deserialized from a format that buffers it first (an
    /// internally tagged enum's fields, YAML), which cannot hand on raw
    /// text: through a tree, re-encoded. JSON text reads with
    /// [`Deserialize`], keeping the text.
    pub fn buffered<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        Self::encode(&value).map_err(serde::de::Error::custom)
    }
}

impl From<Box<RawValue>> for Json {
    fn from(raw: Box<RawValue>) -> Self {
        Self(Arc::from(raw))
    }
}

impl Default for Json {
    fn default() -> Self {
        Self::null()
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
        Box::<RawValue>::deserialize(d).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::Json;

    #[test]
    fn a_payload_is_written_as_the_text_it_was_read_from() {
        let text = r#"{"b": 1,  "a": [true]}"#;
        let json: Json = serde_json::from_str(text).unwrap();
        assert_eq!(json.raw(), text);
        assert_eq!(serde_json::to_string(&json).unwrap(), text);
    }

    #[test]
    fn a_clone_shares_the_text() {
        let json = Json::encode(&vec!["a"; 64]).unwrap();
        let copy = json.clone();
        assert!(std::ptr::eq(json.raw(), copy.raw()));
    }

    #[test]
    fn a_buffered_payload_reads_through_a_tree() {
        #[derive(serde::Deserialize)]
        #[serde(tag = "type")]
        enum Tagged {
            Call {
                #[serde(deserialize_with = "Json::buffered")]
                input: Json,
            },
        }
        let Tagged::Call { input } =
            serde_json::from_str(r#"{"type":"Call","input":{"b":1,"a":2}}"#).unwrap();
        assert_eq!(input.raw(), r#"{"a":2,"b":1}"#);
    }
}
