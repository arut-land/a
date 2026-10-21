//! JSON as providers send it: read leniently, and tool arguments parsed so
//! malformed calls keep their stated input within the decoding ceilings.

use std::collections::VecDeque;
use std::str::FromStr;

use serde_json::{Map, Number, Value};

use crate::error::Error;
use crate::sse::DecodeLimits;

mod read;

pub(crate) use read::{
    Count, Fielded, Fields, First, FromFields, Json, Obj, Word, fields, next, tag, word,
};

/// Strings hide structural punctuation, including across argument fragments.
#[derive(Default)]
pub(crate) struct JsonDepth {
    depth: usize,
    string: bool,
    escaped: bool,
}

impl JsonDepth {
    pub(crate) fn push(&mut self, text: &str, limit: usize) -> Result<(), Error> {
        for byte in text.bytes() {
            if self.string {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.string = false;
                }
            } else {
                match byte {
                    b'"' => self.string = true,
                    b'{' | b'[' => {
                        if self.depth >= limit {
                            return Err(Error::limit("JSON depth"));
                        }
                        self.depth += 1;
                    }
                    b'}' | b']' => self.depth = self.depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn check_text(text: &str, bytes: usize, depth: usize) -> Result<(), Error> {
    if text.len() > bytes {
        return Err(Error::limit("JSON bytes"));
    }
    JsonDepth::default().push(text, depth)
}

#[cfg(test)]
std::thread_local! {
    static ENCODED_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn encoded_work() -> usize {
    ENCODED_WORK.get()
}

/// Count serialized bytes without making another copy of a provider tree.
pub(crate) fn encoded_bytes(value: &impl serde::Serialize, limit: usize) -> Result<usize, Error> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(std::io::Error::other("JSON byte limit"));
            }
            #[cfg(test)]
            ENCODED_WORK.set(ENCODED_WORK.get() + bytes.len());
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| Error::limit("JSON bytes"))?;
    Ok(counter.bytes)
}

/// Each node and string scan is admitted before visiting it. Counting a
/// string's length is constant work; escaping it is not.
pub(crate) fn measured_value(
    value: &Value,
    limits: DecodeLimits,
    work: &mut usize,
) -> Result<usize, Error> {
    fn charge(work: &mut usize, bytes: usize) -> Result<(), Error> {
        *work = work
            .checked_sub(bytes)
            .ok_or_else(|| Error::limit("JSON work"))?;
        Ok(())
    }
    fn string(text: &str, limit: usize, work: &mut usize) -> Result<usize, Error> {
        charge(work, text.len())?;
        encoded_bytes(&text, limit)
    }
    fn visit(value: &Value, left: usize, limit: usize, work: &mut usize) -> Result<usize, Error> {
        charge(work, 1)?;
        let mut bytes: usize = 0;
        let mut add = |more: usize| -> Result<(), Error> {
            bytes = bytes
                .checked_add(more)
                .filter(|size| *size <= limit)
                .ok_or_else(|| Error::limit("JSON bytes"))?;
            Ok(())
        };
        match value {
            Value::Array(values) => {
                let left = left
                    .checked_sub(1)
                    .ok_or_else(|| Error::limit("JSON depth"))?;
                add(2 + values.len().saturating_sub(1))?;
                for value in values {
                    add(visit(value, left, limit, work)?)?;
                }
            }
            Value::Object(values) => {
                let left = left
                    .checked_sub(1)
                    .ok_or_else(|| Error::limit("JSON depth"))?;
                add(2 + values.len().saturating_sub(1))?;
                for (key, value) in values {
                    add(string(key, limit, work)?)?;
                    add(1)?;
                    add(visit(value, left, limit, work)?)?;
                }
            }
            Value::String(text) => add(string(text, limit, work)?)?,
            _ => add(encoded_bytes(value, limit)?)?,
        }
        Ok(bytes)
    }
    visit(value, limits.depth, limits.reply_bytes, work)
}

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

    /// The pointers here are written out and escape nothing, so each token
    /// is the key itself: `Value::pointer` copies every token, twice, to
    /// unescape it.
    fn at(&self, pointer: &str) -> Option<&Value> {
        let mut value = self;
        if !pointer.is_empty() {
            for token in pointer.strip_prefix('/')?.split('/') {
                value = match value {
                    Value::Object(map) => map.get(token)?,
                    // An index as `Value::pointer` reads one: no sign, no
                    // leading zero.
                    Value::Array(items) if !token.starts_with(['+', '0']) || token == "0" => {
                        items.get(token.parse::<usize>().ok()?)?
                    }
                    _ => return None,
                };
            }
        }
        Some(value).filter(|value| !value.is_null())
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

/// An object of `fields`, later ones winning. `Map::from_iter` would sort
/// them first, and each source type it collects brings its own copy of a
/// stable sort (~4 KiB); inserting sorts nothing.
pub(crate) fn object(fields: impl IntoIterator<Item = (String, Value)>) -> Map<String, Value> {
    let mut object = Map::new();
    object.extend(fields);
    object
}

/// A JSON object written as `json!` writes one, each value moved in with
/// `Value::from`. `json!` serializes a copy of every value it interpolates,
/// so a conversation nested in it is copied again at each level.
macro_rules! obj {
    ({ $($key:tt: $value:expr),* $(,)? }) => {
        serde_json::Value::Object($crate::json::object([
            $(($key.to_owned(), serde_json::Value::from($value))),*
        ]))
    };
}
pub(crate) use obj;

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
///
/// The object is written once, compact and its keys in order, whatever
/// spacing and order the model wrote it in: that text is the call's input
/// from here on, the same in the log and in every request that sends it
/// back.
pub(crate) fn arguments(
    text: &str,
    limits: DecodeLimits,
) -> Result<(::json::Json, Option<String>), Error> {
    check_text(text, limits.argument_bytes, limits.depth)?;
    let parsed = if text.trim().is_empty() {
        Ok(Value::Object(Map::new()))
    } else {
        serde_json::from_str(text)
    };
    let (arguments, invalid) = match parsed {
        Ok(Value::Object(arguments)) => (arguments, false),
        Ok(Value::Null) => (Map::new(), false),
        Ok(Value::String(inner)) => {
            check_text(&inner, limits.argument_bytes, limits.depth)?;
            match serde_json::from_str(&inner) {
                Ok(Value::Object(arguments)) => (arguments, false),
                _ => (Map::new(), true),
            }
        }
        Ok(_) => (Map::new(), true),
        Err(_) => (partial_object(text, limits).unwrap_or_default(), true),
    };
    Ok((
        ::json::Json::encode(&arguments).unwrap_or_default(),
        invalid.then(|| text.to_owned()),
    ))
}

/// Whether `text` is a whole JSON object; nothing at all is one.
pub(crate) fn whole_object(text: &str) -> bool {
    text.trim().is_empty() || is_object(text)
}

/// Whether `text` is one JSON object. Text that braces do not bracket is
/// none, so arguments judged at each fragment of a call are parsed only
/// when they may be whole, and the parse builds nothing.
pub(crate) fn is_object(text: &str) -> bool {
    let json = text.trim_matches([' ', '\t', '\n', '\r']);
    json.starts_with('{')
        && json.ends_with('}')
        && serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

const CHECKPOINTS: usize = 256;
type Checkpoint = (usize, Vec<u8>, bool);

fn checkpoint(cuts: &mut VecDeque<Checkpoint>, at: usize, stack: &[u8], quote: bool) {
    let mut open = if cuts.len() == CHECKPOINTS {
        cuts.pop_front().expect("full checkpoints").1
    } else {
        Vec::new()
    };
    open.clear();
    open.extend_from_slice(stack);
    cuts.push_back((at, open, quote));
}

fn checkpoints(text: &str, limits: DecodeLimits) -> Option<VecDeque<Checkpoint>> {
    check_text(text, limits.argument_bytes, limits.depth).ok()?;
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    // Prefix lengths a value just ended at, with the containers open there.
    let mut cuts = VecDeque::with_capacity(CHECKPOINTS);
    let bytes = text.as_bytes();
    for (at, byte) in bytes.iter().enumerate() {
        if in_string {
            match (escaped, *byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => {
                    in_string = false;
                    checkpoint(&mut cuts, at + 1, &stack, false);
                }
                (false, _) => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                stack.push(*byte);
                checkpoint(&mut cuts, at + 1, &stack, false);
            }
            b'}' | b']' => {
                stack.pop();
                checkpoint(&mut cuts, at + 1, &stack, false);
            }
            b if b.is_ascii_alphanumeric() || *b == b'.' || *b == b'-' || *b == b'+' => {
                let next = bytes.get(at + 1).copied();
                if !next.is_some_and(|n| {
                    n.is_ascii_alphanumeric() || n == b'.' || n == b'-' || n == b'+'
                }) {
                    checkpoint(&mut cuts, at + 1, &stack, false);
                }
            }
            _ => {}
        }
    }
    if in_string {
        let at = text.len() - usize::from(escaped);
        checkpoint(&mut cuts, at, &stack, true);
    }
    Some(cuts)
}

pub fn parse_partial_object(text: &str) -> Option<Map<String, Value>> {
    partial_object(text, DecodeLimits::default())
}

fn partial_object(text: &str, limits: DecodeLimits) -> Option<Map<String, Value>> {
    let cuts = checkpoints(text, limits)?;
    // Bound total copying and parsing, even when every recent cut is invalid.
    let mut work = limits.reply_bytes;
    for (at, open, quote) in cuts.iter().rev() {
        let size = at
            .checked_add(open.len())?
            .checked_add(usize::from(*quote))?;
        work = work.checked_sub(size)?;
        let mut candidate = String::with_capacity(size);
        candidate.push_str(text.get(..*at)?);
        if *quote {
            candidate.push('"');
        }
        for container in open.iter().rev() {
            candidate.push(if *container == b'{' { '}' } else { ']' });
        }
        if let Ok(Value::Object(object)) = serde_json::from_str(&candidate) {
            return Some(object);
        }
    }
    None
}

/// The best object a tool call's argument text states so far, for showing a
/// call while it streams. It never fails: complete JSON parses as is, text
/// with raw control characters or invalid escapes is repaired first, a
/// cut-off prefix keeps the longest object it states
/// ([`parse_partial_object`]), and anything else is an empty object, as is
/// a top level that is not an object.
///
/// It is for display only. A call's arguments are what its end states, and
/// only those are validated and executed.
///
/// ```
/// use llm::parse_partial_arguments;
///
/// let partial = parse_partial_arguments(r#"{"path": "notes.md", "content": "Hel"#);
/// assert_eq!(partial["path"], "notes.md");
/// assert_eq!(partial["content"], "Hel");
/// ```
pub fn parse_partial_arguments(text: &str) -> Map<String, Value> {
    let limits = DecodeLimits::default();
    if check_text(text, limits.argument_bytes, limits.depth).is_err() {
        return Map::new();
    }
    if let Ok(value) = serde_json::from_str(text) {
        return match value {
            Value::Object(object) => object,
            _ => Map::new(),
        };
    }
    let Some(repaired) = repair_json(text) else {
        return Map::new();
    };
    let complete = serde_json::from_str(&repaired).ok();
    if let Some(value) = complete {
        return match value {
            Value::Object(object) => object,
            _ => Map::new(),
        };
    }
    let cut = without_partial_escape(&repaired);
    parse_partial_object(cut)
        .or_else(|| parse_partial_object(text))
        .unwrap_or_default()
}

/// `text` with raw control characters inside strings escaped and each
/// backslash that starts no valid escape doubled, as models sometimes write
/// them.
fn repair_json(text: &str) -> Option<String> {
    let mut repaired = String::with_capacity(text.len());
    let mut in_string = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let growth = if in_string {
            match c {
                '\\' => match chars.peek() {
                    Some(_) => 2,
                    None => 1,
                },
                '\n' | '\r' | '\t' => 2,
                c if c.is_control() => 6,
                c => c.len_utf8(),
            }
        } else {
            c.len_utf8()
        };
        if repaired.len().saturating_add(growth) > DecodeLimits::default().argument_bytes {
            return None;
        }
        if !in_string {
            in_string = c == '"';
            repaired.push(c);
            continue;
        }
        match c {
            '"' => {
                in_string = false;
                repaired.push(c);
            }
            '\\' => match chars.peek() {
                Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u') => {
                    repaired.push(c);
                    if let Some(next) = chars.next() {
                        repaired.push(next);
                    }
                }
                // A trailing backslash may begin an escape the next fragment ends.
                None => repaired.push(c),
                Some(_) => repaired.push_str("\\\\"),
            },
            '\n' => repaired.push_str("\\n"),
            '\r' => repaired.push_str("\\r"),
            '\t' => repaired.push_str("\\t"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                // Writing to a `String` cannot fail.
                let _ = write!(repaired, "\\u{:04x}", u32::from(c));
            }
            c => repaired.push(c),
        }
    }
    Some(repaired)
}

/// `text` without a `\u` escape its end cuts short, which would otherwise
/// cost the whole string it ends.
fn without_partial_escape(text: &str) -> &str {
    let bytes = text.as_bytes();
    let tail = bytes.len().saturating_sub(5);
    let Some(at) = (tail..bytes.len())
        .rev()
        .find(|at| bytes.get(*at..*at + 2) == Some(b"\\u"))
    else {
        return text;
    };
    let digits = bytes.get(at + 2..).unwrap_or_default();
    // A preceding backslash run of odd length escapes this backslash itself.
    let escaping = bytes
        .get(..at)
        .unwrap_or_default()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count();
    if digits.len() < 4 && digits.iter().all(u8::is_ascii_hexdigit) && escaping % 2 == 0 {
        text.get(..at).unwrap_or(text)
    } else {
        text
    }
}

#[cfg(test)]
mod tests;
