//! MCP's mirrored routing values must agree with the JSON body, including
//! values that HTTP cannot carry literally.
use crate::Failure;
use base64::Engine;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

pub(crate) fn value(text: &str) -> String {
    if text.trim() != text
        || text
            .bytes()
            .any(|b| !(b == b'\t' || (0x20..=0x7e).contains(&b)))
        || (text.starts_with("=?base64?") && text.ends_with("?="))
    {
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(text)
        )
    } else {
        text.to_owned()
    }
}

pub(crate) fn parameters(schema: &Value, args: &Value) -> Result<HeaderMap, Failure> {
    let mut headers = HeaderMap::new();
    let mut names = std::collections::BTreeSet::new();
    walk(schema, args, true, true, &mut headers, &mut names)?;
    Ok(headers)
}

fn invalid() -> Failure {
    Failure::Protocol("invalid x-mcp-header annotation or value".into())
}

fn walk(
    schema: &Value,
    args: &Value,
    reachable: bool,
    root: bool,
    headers: &mut HeaderMap,
    names: &mut std::collections::BTreeSet<String>,
) -> Result<(), Failure> {
    if let Some(name) = schema.get("x-mcp-header") {
        let name = name.as_str().ok_or_else(invalid)?;
        let kind = schema["type"].as_str().ok_or_else(invalid)?;
        if !reachable
            || root
            || name.is_empty()
            || !matches!(kind, "string" | "integer" | "boolean")
            || HeaderName::from_bytes(name.as_bytes()).is_err()
            || !names.insert(name.to_ascii_lowercase())
        {
            return Err(invalid());
        }
        if !args.is_null() {
            let text = match (kind, args) {
                ("string", Value::String(s)) => s.clone(),
                ("boolean", Value::Bool(b)) => b.to_string(),
                ("integer", Value::Number(n)) => {
                    let n = n
                        .as_i64()
                        .filter(|n| n.unsigned_abs() <= 9_007_199_254_740_991)
                        .ok_or_else(invalid)?;
                    n.to_string()
                }
                _ => return Err(invalid()),
            };
            let name = HeaderName::from_bytes(format!("mcp-param-{name}").as_bytes())
                .map_err(|_| invalid())?;
            headers.insert(
                name,
                HeaderValue::from_str(&value(&text)).map_err(|_| invalid())?,
            );
        }
    }
    if let Some(object) = schema.as_object() {
        for (key, child) in object {
            if key == "properties" {
                if let Some(properties) = child.as_object() {
                    for (name, child) in properties {
                        walk(child, &args[name], reachable, false, headers, names)?;
                    }
                }
            } else if matches!(
                key.as_str(),
                "$defs" | "definitions" | "patternProperties" | "dependentSchemas"
            ) {
                if let Some(children) = child.as_object() {
                    for child in children.values() {
                        walk(child, &Value::Null, false, false, headers, names)?;
                    }
                }
            } else if matches!(
                key.as_str(),
                "items"
                    | "prefixItems"
                    | "contains"
                    | "allOf"
                    | "anyOf"
                    | "oneOf"
                    | "not"
                    | "if"
                    | "then"
                    | "else"
                    | "additionalProperties"
                    | "unevaluatedProperties"
                    | "propertyNames"
            ) {
                if let Some(children) = child.as_array() {
                    for child in children {
                        walk(child, &Value::Null, false, false, headers, names)?;
                    }
                } else {
                    walk(child, &Value::Null, false, false, headers, names)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mirrored_values_cannot_become_http_syntax() {
        for text in ["hello\nworld", " padded ", "世界", "=?base64?literal?="] {
            let encoded = value(text);
            let payload = encoded
                .strip_prefix("=?base64?")
                .unwrap()
                .strip_suffix("?=")
                .unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .unwrap(),
                text.as_bytes()
            );
        }
        assert_eq!(value("us-west1"), "us-west1");
    }

    #[test]
    fn only_unique_primitive_property_paths_can_route_requests() {
        let schema = json!({"type":"object","properties":{"nested":{"type":"object","properties":{"tenant":{"type":"string","x-mcp-header":"Tenant"}}}}});
        let headers = parameters(&schema, &json!({"nested":{"tenant":"hi\nthere"}})).unwrap();
        assert_eq!(headers["mcp-param-tenant"], value("hi\nthere"));
        assert!(parameters(&schema, &json!({})).unwrap().is_empty());
        for schema in [
            json!({"oneOf":[{"properties":{"t":{"type":"string","x-mcp-header":"T"}}}]}),
            json!({"properties":{"a":{"type":"string","x-mcp-header":"T"},"b":{"type":"string","x-mcp-header":"t"}}}),
            json!({"properties":{"a":{"type":"number","x-mcp-header":"T"}}}),
        ] {
            assert!(parameters(&schema, &Value::Null).is_err());
        }
    }
}
