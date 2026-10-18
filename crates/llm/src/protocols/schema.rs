//! JSON Schema rewritten for providers with strict structured-output rules
//! (`OpenAI`, Anthropic), ported from rig: `additionalProperties: false` and
//! every property required, recursing into `$defs`, `properties`, `items`
//! and combinators, with `oneOf` merged into `anyOf`.

use serde_json::{Map, Value};

/// Per-provider deltas.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Sanitize {
    /// `OpenAI` allows no keywords beside `$ref`.
    pub(crate) strip_ref_siblings: bool,
    /// `OpenAI` requires `properties` on every object schema.
    pub(crate) inject_empty_properties: bool,
    /// Anthropic takes no numeric constraints on numbers.
    pub(crate) strip_numeric_constraints: bool,
}

pub(crate) fn sanitize(schema: &mut Value, options: Sanitize) {
    let Value::Object(obj) = schema else {
        return;
    };
    if options.strip_ref_siblings && obj.contains_key("$ref") {
        obj.retain(|key, _| key == "$ref");
        return;
    }
    let is_object =
        obj.get("type").and_then(Value::as_str) == Some("object") || obj.contains_key("properties");
    if options.inject_empty_properties && is_object && !obj.contains_key("properties") {
        obj.insert("properties".to_owned(), Value::Object(Map::new()));
    }
    if is_object && !obj.contains_key("additionalProperties") {
        obj.insert("additionalProperties".to_owned(), Value::Bool(false));
    }
    if let Some(Value::Object(properties)) = obj.get("properties") {
        let keys = properties.keys().cloned().map(Value::String).collect();
        obj.insert("required".to_owned(), Value::Array(keys));
    }
    if options.strip_numeric_constraints
        && matches!(
            obj.get("type").and_then(Value::as_str),
            Some("integer" | "number")
        )
    {
        for key in [
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
        ] {
            obj.remove(key);
        }
    }
    for key in ["$defs", "properties"] {
        if let Some(Value::Object(children)) = obj.get_mut(key) {
            for child in children.values_mut() {
                sanitize(child, options);
            }
        }
    }
    if let Some(items) = obj.get_mut("items") {
        sanitize(items, options);
    }
    // Neither provider takes `oneOf`.
    if let Some(one_of) = obj.remove("oneOf") {
        match obj.get_mut("anyOf") {
            Some(Value::Array(existing)) => {
                if let Value::Array(mut incoming) = one_of {
                    existing.append(&mut incoming);
                }
            }
            _ => {
                obj.insert("anyOf".to_owned(), one_of);
            }
        }
    }
    for key in ["anyOf", "allOf"] {
        if let Some(Value::Array(variants)) = obj.get_mut(key) {
            for variant in variants {
                sanitize(variant, options);
            }
        }
    }
}

/// `OpenAI`'s strict subset.
pub(crate) fn openai(schema: &mut Value) {
    sanitize(
        schema,
        Sanitize {
            strip_ref_siblings: true,
            inject_empty_properties: true,
            strip_numeric_constraints: false,
        },
    );
}

/// Characters other than `[A-Za-z0-9_-]` replaced by `_`, cut to `max`.
pub(crate) fn legal_call_id(id: &str, max: usize) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(max)
        .collect()
}

/// The id a call takes in place of `id`: `fitted`, or a hash of `id` when
/// nothing legal is left of it, which still names its call; `None` when
/// `id` already fits.
pub(crate) fn fit_call_id(id: &str, fitted: &str) -> Option<String> {
    let fitted = if fitted.is_empty() {
        short_hash(id)
    } else {
        fitted.to_owned()
    };
    (fitted != id).then_some(fitted)
}

/// A short stable hash of `text` (cyrb53, base36), for ids with nothing
/// legal in them.
pub(crate) fn short_hash(text: &str) -> String {
    fn base36(mut value: u32) -> String {
        let mut digits = Vec::new();
        loop {
            digits.push(char::from_digit(value % 36, 36).unwrap_or('0'));
            value /= 36;
            if value == 0 {
                break;
            }
        }
        digits.iter().rev().collect()
    }
    let (mut h1, mut h2) = (0xdead_beef_u32, 0x41c6_ce57_u32);
    for unit in text.encode_utf16().map(u32::from) {
        h1 = (h1 ^ unit).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ unit).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}
