//! A strict tool's input schema in the subset Anthropic's constrained
//! decoding compiles, by Anthropic's SDK policy (ported from rig).
//!
//! Strict tools take optional parameters, so declared `required` lists stay.
//! Validation keywords the compiler rejects are moved into descriptions as
//! guidance. A local root `$ref` is inlined and a root `allOf` flattened,
//! since a tool input's root must be a plain `type: object`.

use serde_json::{Map, Value, json};

/// The keywords that hold a schema's definitions.
const DEFINITIONS: [&str; 2] = ["$defs", "definitions"];

/// The string formats Anthropic's strict schemas take.
const STRICT_FORMATS: [&str; 10] = [
    "date-time",
    "time",
    "date",
    "duration",
    "email",
    "hostname",
    "uri",
    "ipv4",
    "ipv6",
    "uuid",
];

pub(super) fn sanitize(schema: &mut Value) {
    let mut original = std::mem::take(schema);
    inline_local_root_reference(&mut original);
    if let Value::Object(root) = &mut original {
        if let Some(all_of) = root.remove("allOf") {
            let mut conflicts = Map::new();
            merge_all_of(root, all_of, &mut conflicts);
            if !conflicts.is_empty() {
                root.insert("rootAllOfConstraints".into(), Value::Object(conflicts));
            }
        }
        if !root.contains_key("type")
            && (root.contains_key("properties") || root.contains_key("$ref"))
        {
            root.insert("type".into(), json!("object"));
        }
    }
    *schema = strict(original);
}

/// Resolve a local root `$ref` into the root, keeping the definitions nested
/// references need and the root's sibling keywords, which JSON Schema
/// applies conjunctively.
fn inline_local_root_reference(schema: &mut Value) {
    let mut seen = std::collections::BTreeSet::new();
    while let Some(reference) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        let target = reference
            .strip_prefix('#')
            .and_then(|pointer| schema.pointer(pointer));
        let (Some(Value::Object(mut referenced)), Some(mut root)) =
            (target.cloned(), schema.as_object().cloned())
        else {
            return;
        };
        if !seen.insert(reference) {
            return;
        }
        root.remove("$ref");
        for keyword in DEFINITIONS {
            if let Some(definitions) = root.remove(keyword) {
                let merged = merge_definitions(definitions, referenced.remove(keyword));
                referenced.insert(keyword.into(), merged);
            }
        }
        merge_siblings(&mut referenced, root);
        *schema = Value::Object(referenced);
    }
}

/// The root's definitions over the inlined schema's: absolute pointers
/// still resolve from the document root.
fn merge_definitions(root: Value, local: Option<Value>) -> Value {
    match (root, local) {
        (Value::Object(root), Some(Value::Object(mut local))) => {
            local.extend(root);
            Value::Object(local)
        }
        (root, _) => root,
    }
}

/// Merge keywords beside a root `$ref` into its resolved object. Unions and
/// conflicting constraints, which a root cannot hold, stay as guidance.
fn merge_siblings(referenced: &mut Map<String, Value>, siblings: Map<String, Value>) {
    let mut conflicts = Map::new();
    for (keyword, sibling) in siblings {
        match keyword.as_str() {
            "properties" => merge_properties(referenced, sibling),
            "required" => merge_required(referenced, sibling),
            "allOf" => merge_all_of(referenced, sibling, &mut conflicts),
            "anyOf" | "oneOf" => {
                conflicts.insert(keyword, sibling);
            }
            // Annotations of the root document: the root's wins.
            "description" | "title" | "$schema" | "$id" | "$comment" | "default" | "examples"
            | "deprecated" | "readOnly" | "writeOnly" => {
                referenced.insert(keyword, sibling);
            }
            _ => match referenced.get(&keyword) {
                None => {
                    referenced.insert(keyword, sibling);
                }
                Some(existing) if *existing == sibling => {}
                Some(_) => {
                    conflicts.insert(keyword, sibling);
                }
            },
        }
    }
    if !conflicts.is_empty() {
        referenced.insert("rootRefSiblingConstraints".into(), Value::Object(conflicts));
    }
}

/// Merge each object branch of a root `allOf` into `schema`; branches that
/// are not objects stay as guidance.
fn merge_all_of(
    schema: &mut Map<String, Value>,
    sibling: Value,
    conflicts: &mut Map<String, Value>,
) {
    let Value::Array(branches) = sibling else {
        conflicts.insert("allOf".into(), sibling);
        return;
    };
    let mut unsupported = Vec::new();
    for branch in branches {
        let Value::Object(mut branch) = branch else {
            unsupported.push(branch);
            continue;
        };
        if branch.contains_key("$ref") {
            for keyword in DEFINITIONS {
                if let Some(definitions) = schema.get(keyword).cloned() {
                    let merged = merge_definitions(definitions, branch.remove(keyword));
                    branch.insert(keyword.into(), merged);
                }
            }
            let mut inlined = Value::Object(branch);
            inline_local_root_reference(&mut inlined);
            if let Value::Object(inlined) = inlined {
                merge_siblings(schema, inlined);
            }
        } else {
            merge_siblings(schema, branch);
        }
    }
    if !unsupported.is_empty() {
        conflicts.insert("allOf".into(), Value::Array(unsupported));
    }
}

/// Merge sibling properties; a property both define must meet both.
fn merge_properties(schema: &mut Map<String, Value>, sibling: Value) {
    let Value::Object(siblings) = sibling else {
        schema.entry("properties").or_insert(sibling);
        return;
    };
    let Value::Object(properties) = schema.entry("properties").or_insert_with(|| json!({})) else {
        return;
    };
    for (name, sibling) in siblings {
        let merged = match properties.remove(&name) {
            None => sibling,
            Some(existing) if existing == sibling => existing,
            Some(existing) => json!({ "allOf": [existing, sibling] }),
        };
        properties.insert(name, merged);
    }
}

/// Merge sibling `required` names, each once.
fn merge_required(schema: &mut Map<String, Value>, sibling: Value) {
    let Value::Array(names) = sibling else {
        schema.entry("required").or_insert(sibling);
        return;
    };
    let Value::Array(required) = schema.entry("required").or_insert_with(|| json!([])) else {
        return;
    };
    for name in names {
        if !required.contains(&name) {
            required.push(name);
        }
    }
}

/// `schema` in the subset strict tool use compiles: objects closed, the
/// supported keywords kept, and every other one appended to the
/// description as `{keyword: value, ...}`.
fn strict(schema: Value) -> Value {
    let Value::Object(mut source) = schema else {
        return schema;
    };
    let each = |schemas: Map<String, Value>| {
        Value::Object(
            schemas
                .into_iter()
                .map(|(name, schema)| (name, strict(schema)))
                .collect(),
        )
    };
    let mut kept = Map::new();
    for keyword in DEFINITIONS {
        match source.remove(keyword) {
            Some(Value::Object(definitions)) => {
                kept.insert(keyword.into(), each(definitions));
            }
            Some(definitions) => {
                source.insert(keyword.into(), definitions);
            }
            None => {}
        }
    }
    if let Some(reference) = source.remove("$ref") {
        kept.insert("$ref".into(), reference);
        return Value::Object(kept);
    }
    let kind = source.remove("type");
    let is = |expected: &str| match &kind {
        Some(Value::String(kind)) => kind == expected,
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some(expected)),
        _ => false,
    };
    match (alternatives(&mut source), &kind) {
        (Some((keyword, variants)), _) => {
            let variants = variants.into_iter().map(strict).collect();
            kept.insert(keyword.into(), Value::Array(variants));
        }
        (None, Some(kind)) => {
            kept.insert("type".into(), kind.clone());
        }
        (None, None) => {}
    }
    if let Some(Value::Array(values)) = source.remove("enum") {
        kept.insert("enum".into(), Value::Array(values));
    }
    if let Some(constant) = source.remove("const") {
        kept.insert("const".into(), constant);
    }
    for keyword in ["description", "title"] {
        if let Some(Value::String(value)) = source.remove(keyword) {
            kept.insert(keyword.into(), Value::String(value));
        }
    }
    let has_properties = source.contains_key("properties");
    if kind.is_none() && has_properties {
        kept.insert("type".into(), json!("object"));
    }
    if is("object") || has_properties {
        let properties = match source.remove("properties") {
            Some(Value::Object(properties)) => each(properties),
            _ => json!({}),
        };
        kept.insert("properties".into(), properties);
        source.remove("additionalProperties");
        kept.insert("additionalProperties".into(), Value::Bool(false));
        if let Some(Value::Array(required)) = source.remove("required") {
            kept.insert("required".into(), Value::Array(required));
        }
    }
    if is("string")
        && let Some(format) = source.remove("format")
    {
        if format
            .as_str()
            .is_some_and(|format| STRICT_FORMATS.contains(&format))
        {
            kept.insert("format".into(), format);
        } else {
            source.insert("format".into(), format);
        }
    }
    if is("array") {
        if let Some(items) = source.remove("items") {
            kept.insert("items".into(), strict(items));
        }
        if let Some(min_items) = source.remove("minItems") {
            if matches!(min_items.as_u64(), Some(0 | 1)) {
                kept.insert("minItems".into(), min_items);
            } else {
                source.insert("minItems".into(), min_items);
            }
        }
    }
    guide(&mut kept, source);
    Value::Object(kept)
}

/// The schema's combinator, taken out of `source`: `oneOf` merges into
/// `anyOf`, which Anthropic takes in its place.
fn alternatives(source: &mut Map<String, Value>) -> Option<(&'static str, Vec<Value>)> {
    match (
        source.remove("anyOf"),
        source.remove("oneOf"),
        source.remove("allOf"),
    ) {
        (Some(Value::Array(variants)), _, _) | (_, Some(Value::Array(variants)), _) => {
            Some(("anyOf", variants))
        }
        (_, _, Some(Value::Array(variants))) => Some(("allOf", variants)),
        _ => None,
    }
}

/// The keywords `source` still holds, appended to `kept`'s description as
/// `{keyword: value, ...}`.
fn guide(kept: &mut Map<String, Value>, source: Map<String, Value>) {
    if source.is_empty() {
        return;
    }
    let hints: Vec<String> = source
        .into_iter()
        .map(|(keyword, value)| match value {
            Value::String(value) => format!("{keyword}: {value}"),
            value => format!("{keyword}: {value}"),
        })
        .collect();
    let suffix = format!("{{{}}}", hints.join(", "));
    match kept.get_mut("description") {
        Some(Value::String(description)) => {
            description.push_str("\n\n");
            description.push_str(&suffix);
        }
        _ => {
            kept.insert("description".into(), Value::String(suffix));
        }
    }
}
