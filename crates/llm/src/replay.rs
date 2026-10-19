//! How an encoder sends back a part its own protocol produced. The writer
//! keeps each part's provider item with a fingerprint of the part's shared
//! fields; while they still match, the item goes back verbatim. A part
//! edited since keeps only the item's identity (its id, its ciphertext), and
//! anything else is rebuilt from the shared fields.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::spec::{AssistantPart, Meta};

/// What an encoder sends for one part.
#[derive(Clone, Debug, PartialEq)]
pub enum Replay {
    /// The provider item, unchanged.
    Item(Value),
    /// The part was edited: rebuild it, keeping these keys of its item.
    Identity(Map<String, Value>),
    Rebuild,
}

/// The shared fields a fingerprint covers.
#[derive(Clone, Copy, Debug)]
pub enum Canon<'a> {
    Text(&'a str),
    Reasoning(&'a str, bool),
    Call(&'a str, &'a Map<String, Value>),
    Other,
}

impl<'a> Canon<'a> {
    fn of(part: &'a AssistantPart) -> Self {
        match part {
            AssistantPart::Text(text) => Self::Text(&text.text),
            AssistantPart::Reasoning(reasoning) => {
                Self::Reasoning(&reasoning.text, reasoning.redacted)
            }
            AssistantPart::ToolCall(call) => Self::Call(&call.name, &call.input),
            AssistantPart::ToolResult(_) | AssistantPart::Media(_) | AssistantPart::Opaque(_) => {
                Self::Other
            }
        }
    }
}

/// FNV-1a, fed as a writer.
struct Fnv(u64);

impl std::io::Write for Fnv {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A stable digest of `canon` (FNV-1a over a fixed projection), so a field
/// added to a part's type never stales stored items.
pub fn fingerprint(canon: Canon<'_>) -> String {
    use std::io::Write as _;
    let mut hash = Fnv(0xcbf2_9ce4_8422_2325);
    // Writing to a hash cannot fail.
    let _ = match canon {
        Canon::Text(text) => write!(hash, "t\0{text}"),
        Canon::Reasoning(text, redacted) => write!(hash, "r\0{redacted}\0{text}"),
        Canon::Call(name, input) => write!(hash, "c\0{name}\0")
            .and_then(|()| hash.write_all(serde_json::to_string(input)?.as_bytes())),
        Canon::Other => return String::new(),
    };
    format!("{:016x}", hash.0)
}

/// A part's state under its protocol's key, as the writer keeps it: the
/// provider item, with the fingerprint of the shared fields it was decoded
/// to or, for an opaque part, whether it goes back. Its fields are in key
/// order, as a tree of it would write them.
#[derive(Serialize)]
pub(crate) struct Written<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fp: Option<&'a str>,
    pub item: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay: Option<bool>,
}

/// [`Written`] state as an encoder reads it, the item left as its text.
#[derive(Deserialize)]
struct Kept<'a> {
    #[serde(default, borrow)]
    fp: Option<&'a str>,
    #[serde(default, borrow)]
    item: Option<&'a RawValue>,
    #[serde(default)]
    replay: Option<bool>,
}

/// The provider item `meta` keeps under `key`; `None` when it keeps none.
pub(crate) fn item(meta: &Meta, key: &str) -> Option<Value> {
    let item = meta.parse::<Kept<'_>>(key)?.item?;
    serde_json::from_str(item.get()).ok()
}

/// What to send for `part` under protocol `key`. `identity` picks the keys
/// of a stale item that a rebuilt part keeps.
pub fn replay(
    part: &AssistantPart,
    key: &str,
    identity: impl FnOnce(&Value) -> Map<String, Value>,
) -> Replay {
    let Some(state) = part.meta().parse::<Kept<'_>>(key) else {
        return Replay::Rebuild;
    };
    let Some(item) = state
        .item
        .and_then(|item| serde_json::from_str(item.get()).ok())
    else {
        return Replay::Rebuild;
    };
    let current = match Canon::of(part) {
        Canon::Other => true,
        canon => state.fp == Some(fingerprint(canon).as_str()),
    };
    if current {
        return Replay::Item(item);
    }
    let kept = identity(&item);
    if kept.is_empty() {
        Replay::Rebuild
    } else {
        Replay::Identity(kept)
    }
}

/// Whether the opaque `part` goes back under `key`.
pub fn replays(part: &AssistantPart, key: &str) -> bool {
    let state = part.meta().parse::<Kept<'_>>(key);
    state.and_then(|state| state.replay).unwrap_or(false)
}

#[cfg(test)]
mod tests;
