//! How streamed fragments of a provider object merge into what arrived
//! so far: a reasoning part's details, a tool call's fields, a message's
//! content parts.

use serde_json::{Map, Value, json};

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

/// The keys whose string fragments concatenate when a provider streams
/// them: text and reasoning, a call's arguments or custom input, audio's
/// transcript and data, a reasoning detail's text and summary. Every other
/// string is an identifier, a tag or a signature a later fragment restates.
const FRAGMENT_KEYS: [&str; 13] = [
    "content",
    "input",
    "refusal",
    "reasoning",
    "reasoning_content",
    "reasoning_text",
    "thinking",
    "tool_plan",
    "transcript",
    "data",
    "arguments",
    "text",
    "summary",
];

/// One streamed fragment of a provider object merged into what arrived so
/// far: fragment strings append, arrays extend, objects merge key by key,
/// and content parts (a message's `content`, a thinking part's `thinking`)
/// merge as parts. A value never replaces one of another JSON type, a
/// `null` or empty string never erases a value, and a literal `null`
/// argument placeholder gives way to the first real fragment.
pub(super) fn merge_fields(target: &mut Map<String, Value>, delta: &Map<String, Value>) {
    for (key, value) in delta {
        let fragment = FRAGMENT_KEYS.contains(&key.as_str());
        match (target.get_mut(key), value) {
            (Some(_), Value::Null) => {}
            (Some(existing), more)
                if matches!(key.as_str(), "content" | "thinking")
                    && (existing.is_array() || more.is_array()) =>
            {
                merge_content(existing, more);
            }
            (Some(Value::String(existing)), Value::String(more)) if fragment => {
                if existing.trim() == "null" && !more.trim().is_empty() {
                    existing.clear();
                }
                existing.push_str(more);
            }
            (Some(Value::String(existing)), Value::String(more))
                if more.is_empty() && !existing.is_empty() => {}
            (Some(Value::Array(existing)), Value::Array(more)) => {
                existing.extend(more.iter().cloned());
            }
            (Some(Value::Object(existing)), Value::Object(more)) => merge_fields(existing, more),
            (Some(existing), more)
                if !existing.is_null()
                    && std::mem::discriminant(existing) != std::mem::discriminant(more) => {}
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Streamed content merged into what arrived so far, as parts once either
/// side is a part array: a string is a text part, and a text or thinking
/// part continues the last part of its type.
fn merge_content(existing: &mut Value, more: &Value) {
    fn parts(value: Value) -> Vec<Value> {
        match value {
            Value::Array(parts) => parts,
            Value::String(text) if !text.is_empty() => vec![json!({"type": "text", "text": text})],
            _ => Vec::new(),
        }
    }
    let mut merged = parts(std::mem::take(existing));
    for part in parts(more.clone()) {
        let kind = part.str("type").map(str::to_owned);
        let kind = kind.as_deref();
        match (merged.last_mut(), part) {
            (Some(Value::Object(last)), Value::Object(next))
                if matches!(kind, Some("text" | "thinking"))
                    && last.get("type").and_then(Value::as_str) == kind =>
            {
                merge_fields(last, &next);
            }
            (_, part) => merged.push(part),
        }
    }
    *existing = Value::Array(merged);
}
