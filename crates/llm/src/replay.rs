//! How an encoder sends back a part its own protocol produced. The writer
//! keeps each part's provider item with a fingerprint of the part's shared
//! fields; while they still match, the item goes back verbatim. A part
//! edited since keeps only the item's identity (its id, its ciphertext), and
//! anything else is rebuilt from the shared fields.

use serde_json::{Map, Value};

use crate::spec::AssistantPart;

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

/// What to send for `part` under protocol `key`. `identity` picks the keys
/// of a stale item that a rebuilt part keeps.
pub fn replay(
    part: &AssistantPart,
    key: &str,
    identity: impl FnOnce(&Value) -> Map<String, Value>,
) -> Replay {
    let Some(mut state) = part.meta().get(key) else {
        return Replay::Rebuild;
    };
    let item = state.get_mut("item").map(Value::take);
    let Some(item) = item.filter(|item| !item.is_null()) else {
        return Replay::Rebuild;
    };
    let current = match Canon::of(part) {
        Canon::Other => true,
        canon => state.get("fp").and_then(Value::as_str) == Some(fingerprint(canon).as_str()),
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
    part.meta()
        .field(key, "replay")
        .and_then(|replay| replay.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
