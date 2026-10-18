//! How an encoder sends back a part its own protocol produced. The writer
//! keeps each part's provider item with a fingerprint of the part's shared
//! fields; while they still match, the item goes back verbatim. A part
//! edited since keeps only the item's identity (its id, its ciphertext), and
//! anything else is rebuilt from the shared fields.

use serde_json::{Map, Value};

use crate::spec::AssistantPart;

/// What an encoder sends for one part.
#[derive(Clone, Debug, PartialEq)]
pub enum Replay<'a> {
    /// The provider item, unchanged.
    Item(&'a Value),
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

impl Fnv {
    fn eat(&mut self, parts: &[&[u8]]) {
        for byte in parts.iter().copied().flatten() {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

impl std::io::Write for Fnv {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.eat(&[bytes]);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A stable digest of `canon` (FNV-1a over a fixed projection), so a field
/// added to a part's type never stales stored items.
pub fn fingerprint(canon: Canon<'_>) -> String {
    let mut hash = Fnv(0xcbf2_9ce4_8422_2325);
    match canon {
        Canon::Text(text) => hash.eat(&[b"t\0", text.as_bytes()]),
        Canon::Reasoning(text, redacted) => {
            let redacted = if redacted { "true" } else { "false" };
            hash.eat(&[b"r\0", redacted.as_bytes(), b"\0", text.as_bytes()]);
        }
        Canon::Call(name, input) => {
            hash.eat(&[b"c\0", name.as_bytes(), b"\0"]);
            // Writing to a hash cannot fail.
            let _ = serde_json::to_writer(&mut hash, input);
        }
        Canon::Other => return String::new(),
    }
    format!("{:016x}", hash.0)
}

/// What to send for `part` under protocol `key`. `identity` picks the keys
/// of a stale item that a rebuilt part keeps.
pub fn replay<'a>(
    part: &'a AssistantPart,
    key: &str,
    identity: impl FnOnce(&Value) -> Map<String, Value>,
) -> Replay<'a> {
    let Some(state) = part.meta().get(key) else {
        return Replay::Rebuild;
    };
    let Some(item) = state.get("item").filter(|item| !item.is_null()) else {
        return Replay::Rebuild;
    };
    let current = match Canon::of(part) {
        Canon::Other => true,
        canon => state.get("fp").and_then(Value::as_str) == Some(fingerprint(canon).as_str()),
    };
    if current {
        return Replay::Item(item);
    }
    let kept = identity(item);
    if kept.is_empty() {
        Replay::Rebuild
    } else {
        Replay::Identity(kept)
    }
}

/// Whether the opaque `part` goes back under `key`.
pub fn replays(part: &AssistantPart, key: &str) -> bool {
    part.meta()
        .get(key)
        .and_then(|state| state.get("replay"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fingerprints are stored in session logs: they never change.
    #[test]
    fn fingerprints_are_stable() {
        let input = serde_json::json!({"b": [true, null], "a": 1});
        let input = input.as_object().unwrap();
        assert_eq!(fingerprint(Canon::Text("hi")), "cad93eee6ddd9cfe");
        assert_eq!(fingerprint(Canon::Reasoning("x", true)), "fddb7adfc2dd3d73");
        assert_eq!(fingerprint(Canon::Call("f", input)), "0f90db0a3e870785");
        assert_eq!(fingerprint(Canon::Other), "");
    }
}
