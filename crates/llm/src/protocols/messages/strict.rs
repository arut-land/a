//! A strict tool's input schema in the subset Anthropic's constrained
//! decoding compiles, by Anthropic's SDK policy (ported from rig).
//!
//! Strict tools take optional parameters, so declared `required` lists stay.
//! Validation keywords the compiler rejects are moved into descriptions as
//! guidance. A local root `$ref` is inlined and a root `allOf` flattened,
//! since a tool input's root must be a plain `type: object`.

use serde_json::{Map, Value, json};

type Schema = Map<String, Value>;

/// The keywords that hold a schema's definitions.
const DEFINITIONS: [&str; 2] = ["$defs", "definitions"];

pub(super) fn sanitize(schema: &mut Value) {
    inline_root_ref(schema);
    if let Value::Object(root) = schema {
        if let Some(all_of) = root.remove("allOf") {
            let mut conflicts = Map::new();
            merge_all_of(root, all_of, &mut conflicts);
            keep_conflicts(root, "rootAllOfConstraints", conflicts);
        }
        if let [false, true, _] | [false, _, true] =
            ["type", "properties", "$ref"].map(|key| root.contains_key(key))
        {
            root.insert("type".into(), "object".into());
        }
    }
    *schema = strict(std::mem::take(schema));
}

/// Resolve a local root `$ref` into the root, keeping the definitions nested
/// references need and the root's sibling keywords, which JSON Schema
/// applies conjunctively.
fn inline_root_ref(schema: &mut Value) {
    let mut seen = std::collections::BTreeSet::new();
    while let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target = reference
            .strip_prefix('#')
            .and_then(|at| schema.pointer(at));
        let Some(Value::Object(mut referenced)) = target.cloned() else {
            return;
        };
        if !seen.insert(reference.to_owned()) {
            return;
        }
        let Value::Object(mut root) = std::mem::take(schema) else {
            return;
        };
        root.remove("$ref");
        adopt_definitions(&mut referenced, |keyword| root.remove(keyword));
        merge_siblings(&mut referenced, root);
        *schema = Value::Object(referenced);
    }
}

/// `outer`'s definitions over `schema`'s: absolute pointers still resolve
/// from the document root.
fn adopt_definitions(schema: &mut Schema, mut outer: impl FnMut(&str) -> Option<Value>) {
    for keyword in DEFINITIONS {
        let Some(outer) = outer(keyword) else {
            continue;
        };
        let merged = match (outer, schema.remove(keyword)) {
            (Value::Object(outer), Some(Value::Object(mut local))) => {
                local.extend(outer);
                Value::Object(local)
            }
            (outer, _) => outer,
        };
        schema.insert(keyword.into(), merged);
    }
}

/// `conflicts`, the constraints a root cannot hold, kept as guidance.
fn keep_conflicts(schema: &mut Schema, key: &str, conflicts: Schema) {
    if !conflicts.is_empty() {
        schema.insert(key.into(), Value::Object(conflicts));
    }
}

/// Merge keywords beside a root `$ref` into its resolved object. Unions and
/// conflicting constraints stay as guidance.
fn merge_siblings(schema: &mut Schema, siblings: Schema) {
    let mut conflicts = Map::new();
    for (keyword, sibling) in siblings {
        match keyword.as_str() {
            "properties" => merge_properties(schema, sibling),
            "required" => merge_required(schema, sibling),
            "allOf" => merge_all_of(schema, sibling, &mut conflicts),
            "anyOf" | "oneOf" => {
                conflicts.insert(keyword, sibling);
            }
            // Annotations of the root document: the root's wins.
            "description" | "title" | "$schema" | "$id" | "$comment" | "default" | "examples"
            | "deprecated" | "readOnly" | "writeOnly" => {
                schema.insert(keyword, sibling);
            }
            _ if schema
                .get(&keyword)
                .is_none_or(|existing| *existing == sibling) =>
            {
                schema.insert(keyword, sibling);
            }
            _ => {
                conflicts.insert(keyword, sibling);
            }
        }
    }
    keep_conflicts(schema, "rootRefSiblingConstraints", conflicts);
}

/// Merge each object branch of a root `allOf` into `schema`; branches that
/// are not objects stay as guidance.
fn merge_all_of(schema: &mut Schema, all_of: Value, conflicts: &mut Schema) {
    let Value::Array(branches) = all_of else {
        conflicts.insert("allOf".into(), all_of);
        return;
    };
    let mut unsupported = Vec::new();
    for branch in branches {
        match branch {
            Value::Object(mut branch) if branch.contains_key("$ref") => {
                adopt_definitions(&mut branch, |keyword| schema.get(keyword).cloned());
                let mut inlined = Value::Object(branch);
                inline_root_ref(&mut inlined);
                if let Value::Object(inlined) = inlined {
                    merge_siblings(schema, inlined);
                }
            }
            Value::Object(branch) => merge_siblings(schema, branch),
            branch => unsupported.push(branch),
        }
    }
    if !unsupported.is_empty() {
        conflicts.insert("allOf".into(), Value::Array(unsupported));
    }
}

/// Merge sibling properties; a property both define must meet both.
fn merge_properties(schema: &mut Schema, sibling: Value) {
    let Value::Object(siblings) = sibling else {
        schema.entry("properties").or_insert(sibling);
        return;
    };
    if let Value::Object(properties) = schema.entry("properties").or_insert_with(|| json!({})) {
        for (name, sibling) in siblings {
            let merged = match properties.remove(&name) {
                Some(existing) if existing != sibling => json!({ "allOf": [existing, sibling] }),
                _ => sibling,
            };
            properties.insert(name, merged);
        }
    }
}

/// Merge sibling `required` names, each once.
fn merge_required(schema: &mut Schema, sibling: Value) {
    let Value::Array(names) = sibling else {
        schema.entry("required").or_insert(sibling);
        return;
    };
    if let Value::Array(required) = schema.entry("required").or_insert_with(|| json!([])) {
        for name in names {
            if !required.contains(&name) {
                required.push(name);
            }
        }
    }
}

/// Each of `schemas` strict.
fn each(schemas: Schema) -> Value {
    Value::Object(
        schemas
            .into_iter()
            .map(|(name, schema)| (name, strict(schema)))
            .collect(),
    )
}

/// `schema` in the subset strict tool use compiles: objects closed, the
/// supported keywords kept, a few of the wrong type dropped, and every
/// other one appended to the description as `{keyword: value, ...}`.
fn strict(schema: Value) -> Value {
    let Value::Object(mut source) = schema else {
        return schema;
    };
    let mut kept = Map::new();
    if source.contains_key("$ref") {
        // A reference keeps only its definitions.
        for (keyword, value) in source {
            match (keyword.as_str(), value) {
                ("$defs" | "definitions", Value::Object(definitions)) => {
                    kept.insert(keyword, each(definitions));
                }
                ("$ref", reference) => {
                    kept.insert(keyword, reference);
                }
                _ => {}
            }
        }
        return Value::Object(kept);
    }
    let kind = source.remove("type");
    let is = |expected: &str| match &kind {
        Some(Value::String(kind)) => kind == expected,
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some(expected)),
        _ => false,
    };
    // `oneOf` merges into `anyOf`, which Anthropic takes in its place.
    let shape = match ["anyOf", "oneOf", "allOf"].map(|keyword| source.remove(keyword)) {
        [Some(Value::Array(variants)), ..] | [_, Some(Value::Array(variants)), _] => {
            Some(("anyOf", variants.into_iter().map(strict).collect()))
        }
        [.., Some(Value::Array(variants))] => {
            Some(("allOf", variants.into_iter().map(strict).collect()))
        }
        _ => kind.clone().map(|kind| ("type", kind)),
    };
    kept.extend(shape.map(|(keyword, shape)| (keyword.to_owned(), shape)));
    let has_properties = source.contains_key("properties");
    let object = is("object") || has_properties;
    if object {
        kept.insert("properties".into(), json!({}));
        kept.insert("additionalProperties".into(), false.into());
        if kind.is_none() && has_properties {
            kept.insert("type".into(), "object".into());
        }
    }
    let format = |format: &Value| {
        let formats = ["date-time", "time", "date", "duration", "email", "hostname"];
        let more = ["uri", "ipv4", "ipv6", "uuid"];
        format
            .as_str()
            .is_some_and(|f| formats.contains(&f) || more.contains(&f))
    };
    let mut guided = Map::new();
    for (keyword, value) in source {
        // `Some` keeps, `None` drops, `Err` guides.
        let sent = match (keyword.as_str(), value) {
            ("$defs" | "definitions", Value::Object(definitions)) => Ok(Some(each(definitions))),
            ("const", value)
            | ("enum", value @ Value::Array(_))
            | ("description" | "title", value @ Value::String(_)) => Ok(Some(value)),
            ("properties", Value::Object(properties)) => Ok(Some(each(properties))),
            ("enum" | "description" | "title" | "properties", _) => Ok(None),
            ("required", value @ Value::Array(_)) if object => Ok(Some(value)),
            ("additionalProperties" | "required", _) if object => Ok(None),
            ("format", value) if is("string") && format(&value) => Ok(Some(value)),
            ("items", value) if is("array") => Ok(Some(strict(value))),
            ("minItems", value) if is("array") && matches!(value.as_u64(), Some(0 | 1)) => {
                Ok(Some(value))
            }
            (_, value) => Err(value),
        };
        match sent {
            Ok(Some(value)) => kept.insert(keyword, value),
            Ok(None) => None,
            Err(value) => guided.insert(keyword, value),
        };
    }
    guide(&mut kept, &guided);
    Value::Object(kept)
}

/// The keywords `source` still holds, appended to `kept`'s description as
/// `{keyword: value, ...}`.
fn guide(kept: &mut Schema, source: &Schema) {
    if source.is_empty() {
        return;
    }
    let hints: Vec<String> = source
        .iter()
        .map(|(keyword, value)| match value.as_str() {
            Some(text) => format!("{keyword}: {text}"),
            None => format!("{keyword}: {value}"),
        })
        .collect();
    let hints = format!("{{{}}}", hints.join(", "));
    let description = match kept.remove("description") {
        Some(Value::String(description)) => format!("{description}\n\n{hints}"),
        _ => hints,
    };
    kept.insert("description".into(), description.into());
}
