//! JSON as providers send it: read leniently, and tool arguments parsed so
//! a malformed or cut-off call never fails a reply.

use std::str::FromStr;

use serde_json::{Map, Number, Value};

mod read;

pub(crate) use read::{Json, Str, first, lax, present, tag, word};

/// Lenient reads of a JSON value: a missing field or one of the wrong type
/// is `None`, and numbers may arrive as strings.
pub trait Lenient {
    fn str(&self, key: &str) -> Option<&str>;
    fn u64(&self, key: &str) -> Option<u64>;
    fn obj(&self, key: &str) -> Option<&Map<String, Value>>;
    /// The array at `key`, empty when absent or of another type.
    fn arr(&self, key: &str) -> &[Value];
    /// The value at a JSON pointer, `None` for `null`.
    fn at(&self, pointer: &str) -> Option<&Value>;
    fn as_u64_lenient(&self) -> Option<u64>;
}

impl Lenient for Value {
    fn str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    fn u64(&self, key: &str) -> Option<u64> {
        self.get(key)?.as_u64_lenient()
    }

    fn obj(&self, key: &str) -> Option<&Map<String, Value>> {
        self.get(key)?.as_object()
    }

    fn arr(&self, key: &str) -> &[Value] {
        self.get(key)
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    fn at(&self, pointer: &str) -> Option<&Value> {
        self.pointer(pointer).filter(|value| !value.is_null())
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn as_u64_lenient(&self) -> Option<u64> {
        number(self, |number| {
            number.as_u64().or_else(|| {
                number
                    .as_f64()
                    .filter(|float| *float >= 0.0 && float.fract() == 0.0)
                    .map(|float| float as u64)
            })
        })
    }
}

/// A number, or a string that parses as one.
fn number<T: FromStr>(value: &Value, read: impl FnOnce(&Number) -> Option<T>) -> Option<T> {
    match value {
        Value::Number(number) => read(number),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// The sum of the counts in an object of counters (`{"web_search": 2}`),
/// or of objects holding them under `key` (`{"web_search": {"num_requests":
/// 2}}`): `None` when it holds none.
pub(crate) fn counters(object: Option<&Value>, key: Option<&str>) -> Option<u64> {
    object?
        .as_object()?
        .values()
        .filter_map(|value| match key {
            Some(key) => value.get(key)?.as_u64_lenient(),
            None => value.as_u64_lenient(),
        })
        .fold(None, |sum, count| Some(sum.unwrap_or(0) + count))
}

/// `overlay` deep-merged into `body`: objects merge key by key, anything
/// else replaces, and `null` removes.
pub fn overlay(body: &mut Value, overlay: &Map<String, Value>) {
    let Value::Object(map) = body else {
        *body = Value::Object(overlay.clone());
        return;
    };
    for (key, value) in overlay {
        match (map.get_mut(key), value) {
            (_, Value::Null) => {
                map.remove(key);
            }
            (Some(existing @ Value::Object(_)), Value::Object(inner)) => {
                self::overlay(existing, inner);
            }
            _ => {
                map.insert(key.clone(), value.clone());
            }
        }
    }
}

/// A call's arguments from the text it ends with: the object it states and,
/// when it states none, the text itself. Text that is not an object keeps
/// what a cut-off object still states, or `{}`; a string holding an object
/// is unwrapped once; `null` is no arguments.
pub fn arguments(text: &str) -> (Map<String, Value>, Option<String>) {
    let parsed = if text.trim().is_empty() {
        Ok(Value::Object(Map::new()))
    } else {
        serde_json::from_str(text)
    };
    let (arguments, invalid) = match parsed {
        Ok(Value::Object(arguments)) => (arguments, false),
        Ok(Value::Null) => (Map::new(), false),
        Ok(Value::String(inner)) => match serde_json::from_str(&inner) {
            Ok(Value::Object(arguments)) => (arguments, false),
            _ => (Map::new(), true),
        },
        Ok(_) => (Map::new(), true),
        Err(_) => (parse_partial_object(text).unwrap_or_default(), true),
    };
    (arguments, invalid.then(|| text.to_owned()))
}

/// Whether `text` is a whole JSON object; nothing at all is one.
pub(crate) fn whole_object(text: &str) -> bool {
    text.trim().is_empty() || matches!(serde_json::from_str(text), Ok(Value::Object(_)))
}

/// The longest object a cut-off prefix of JSON still states, closed where
/// it was cut; `None` when it states none.
fn parse_partial_object(text: &str) -> Option<Map<String, Value>> {
    // Bounds the work on a large cut-off input: the last valid cut is
    // almost always near its end.
    const MAX_ATTEMPTS: usize = 256;
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    // Prefix lengths a value just ended at, with the containers open there.
    let mut cuts: Vec<(usize, Vec<u8>, bool)> = Vec::new();
    let bytes = text.as_bytes();
    for (at, byte) in bytes.iter().enumerate() {
        if in_string {
            match (escaped, *byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => {
                    in_string = false;
                    cuts.push((at + 1, stack.clone(), false));
                }
                (false, _) => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                stack.push(*byte);
                cuts.push((at + 1, stack.clone(), false));
            }
            b'}' | b']' => {
                stack.pop();
                cuts.push((at + 1, stack.clone(), false));
            }
            b if b.is_ascii_alphanumeric() || *b == b'.' || *b == b'-' || *b == b'+' => {
                let next = bytes.get(at + 1).copied();
                if !next.is_some_and(|n| {
                    n.is_ascii_alphanumeric() || n == b'.' || n == b'-' || n == b'+'
                }) {
                    cuts.push((at + 1, stack.clone(), false));
                }
            }
            _ => {}
        }
    }
    if in_string {
        let at = text.len() - usize::from(escaped);
        cuts.push((at, stack.clone(), true));
    }
    cuts.iter()
        .rev()
        .take(MAX_ATTEMPTS)
        .find_map(|(at, open, quote)| {
            let mut candidate = text.get(..*at)?.to_owned();
            if *quote {
                candidate.push('"');
            }
            for container in open.iter().rev() {
                candidate.push(if *container == b'{' { '}' } else { ']' });
            }
            match serde_json::from_str(&candidate) {
                Ok(Value::Object(object)) => Some(object),
                _ => None,
            }
        })
}

#[cfg(test)]
mod tests;
