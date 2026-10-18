//! How streamed fragments of a provider object merge into what arrived
//! so far: a reasoning part's details, a tool call's fields, a message's
//! content parts.

use serde_json::{Map, Value};

use super::PartKind;
use crate::json::Lenient;

/// The structured reasoning key, kept verbatim with the reasoning text.
pub(super) const REASONING_DETAILS: &str = "reasoning_details";

/// Streamed reasoning details appended to the part's: a text or summary
/// fragment continues the last entry of its type and index, appending its
/// text and filling the fields that entry lacks; an encrypted entry stays
/// whole.
pub(super) fn merge_details(block: &mut Map<String, Value>, details: Vec<Value>) {
    let Value::Array(merged) = block
        .entry(REASONING_DETAILS)
        .or_insert_with(|| Value::Array(Vec::new()))
    else {
        return;
    };
    for detail in details {
        let kind = detail.str("type");
        let continues = matches!(kind, Some("reasoning.text" | "reasoning.summary"))
            && merged.last().is_some_and(|last| {
                last.str("type") == kind && last.get("index") == detail.get("index")
            });
        match (continues, merged.last_mut(), detail) {
            (true, Some(Value::Object(last)), Value::Object(fields)) => {
                for (key, value) in fields {
                    let missing = last
                        .get(&key)
                        .is_none_or(|existing| existing.is_null() || existing.as_str() == Some(""));
                    match (last.get_mut(&key), value) {
                        (Some(Value::String(text)), Value::String(more))
                            if matches!(key.as_str(), "text" | "summary") =>
                        {
                            text.push_str(&more);
                        }
                        (_, value) if missing => {
                            last.insert(key, value);
                        }
                        _ => {}
                    }
                }
            }
            (_, _, detail) => merged.push(detail),
        }
    }
}

/// Whether strings under `key` concatenate when a provider streams them:
/// text and reasoning, a call's arguments or custom input, audio's
/// transcript and data, a reasoning detail's text and summary. Every other
/// string is an identifier, a tag or a signature a later fragment restates.
fn fragment(key: &str) -> bool {
    matches!(
        key,
        "content"
            | "input"
            | "refusal"
            | "reasoning"
            | "reasoning_content"
            | "reasoning_text"
            | "thinking"
            | "tool_plan"
            | "transcript"
            | "data"
            | "arguments"
            | "text"
            | "summary"
    )
}

/// A string fragment appended; a literal `null` argument placeholder gives
/// way to the first real fragment.
fn append(existing: &mut String, more: &str) {
    if existing.trim() == "null" && !more.trim().is_empty() {
        existing.clear();
    }
    existing.push_str(more);
}

/// One streamed fragment of a provider object merged into what arrived so
/// far: fragment strings append, arrays extend, objects merge key by key,
/// and content parts (a message's `content`, a thinking part's `thinking`)
/// merge as parts. A value never replaces one of another JSON type, and a
/// `null` or empty string never erases a value.
pub(super) fn merge_fields<'a, K: AsRef<str>>(
    target: &mut Map<String, Value>,
    delta: impl IntoIterator<Item = (K, &'a Value)>,
) {
    for (key, value) in delta {
        let key = key.as_ref();
        match (target.get_mut(key), value) {
            (Some(_), Value::Null) => {}
            (Some(existing), more)
                if matches!(key, "content" | "thinking")
                    && (existing.is_array() || more.is_array()) =>
            {
                merge_content(existing, more);
            }
            (_, Value::String(more)) => merge_str(target, key, more),
            (Some(Value::Array(existing)), Value::Array(more)) => {
                existing.extend(more.iter().cloned());
            }
            (Some(Value::Object(existing)), Value::Object(more)) => merge_fields(existing, more),
            (Some(existing), more)
                if !existing.is_null()
                    && std::mem::discriminant(existing) != std::mem::discriminant(more) => {}
            _ => {
                target.insert(key.to_owned(), value.clone());
            }
        }
    }
}

/// A string merged as [`merge_fields`] merges one, without building its
/// value first: a fragment appends, an empty string never erases one, and
/// a string never replaces a value of another JSON type.
pub(super) fn merge_str(target: &mut Map<String, Value>, key: &str, more: &str) {
    match target.get_mut(key) {
        Some(Value::String(existing)) if fragment(key) => append(existing, more),
        Some(Value::String(existing)) if more.is_empty() && !existing.is_empty() => {}
        Some(existing @ (Value::Null | Value::String(_))) => *existing = more.into(),
        Some(_) => {}
        None => {
            target.insert(key.to_owned(), more.into());
        }
    }
}

/// An object merged under `key` as [`merge_fields`] merges one, its fields
/// by `merge`.
pub(super) fn merge_object(
    target: &mut Map<String, Value>,
    key: &str,
    merge: impl FnOnce(&mut Map<String, Value>),
) {
    match target.get_mut(key) {
        Some(Value::Object(existing)) => merge(existing),
        None | Some(Value::Null) => {
            let mut object = Map::new();
            merge(&mut object);
            target.insert(key.to_owned(), Value::Object(object));
        }
        Some(_) => {}
    }
}

/// Streamed content merged into what arrived so far, as parts once either
/// side is a part array: a string is a text part, and a text or thinking
/// part continues the last part of its type.
fn merge_content(existing: &mut Value, more: &Value) {
    fn parts(value: Value) -> Vec<Value> {
        match value {
            Value::Array(parts) => parts,
            Value::String(text) if !text.is_empty() => vec![PartKind::Text.part(text.into())],
            _ => Vec::new(),
        }
    }
    let mut merged = parts(std::mem::take(existing));
    for part in parts(more.clone()) {
        let joins = matches!(
            PartKind::of(&part),
            Some(PartKind::Text | PartKind::Thinking)
        );
        match (merged.last_mut(), part) {
            (Some(Value::Object(last)), Value::Object(next))
                if joins && last.get("type") == next.get("type") =>
            {
                merge_fields(last, &next);
            }
            (_, part) => merged.push(part),
        }
    }
    *existing = Value::Array(merged);
}
