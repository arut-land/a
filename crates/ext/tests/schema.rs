//! Tool input schemas, derived from the type.
use serde::Deserialize;
use serde_json::{Value, json};

use ext::Schema;
use ext::schema::{of, property};

#[derive(Deserialize, Schema)]
#[allow(dead_code)]
struct Replace {
    /// The exact text to replace.
    old: String,
    new: String,
}

#[derive(Deserialize, Schema)]
#[allow(dead_code)]
struct Input {
    /// The command line.
    command: String,
    /// Seconds before it stops; "120" by default.
    #[serde(default)]
    #[schema(min = 1, max = 600)]
    timeout: Option<u64>,
    #[serde(default, rename = "bg")]
    background: bool,
    maybe: Option<String>,
    #[schema(min_items = 1)]
    edits: Vec<Replace>,
}

/// A schema is the type's: each field a property, described by its doc
/// comment, bounded as its attribute says, required unless it has a
/// default or may be left out; and nothing else allowed.
#[test]
fn a_tool_schema_is_derived_not_stored() {
    let schema: Value = serde_json::from_str(&of::<Input>()).unwrap();
    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line." },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600,
                    "description": "Seconds before it stops; \"120\" by default."
                },
                "bg": { "type": "boolean" },
                "maybe": { "type": "string" },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old": { "type": "string", "description": "The exact text to replace." },
                            "new": { "type": "string" }
                        },
                        "required": ["old", "new"],
                        "additionalProperties": false
                    },
                    "minItems": 1
                }
            },
            "required": ["command", "edits"],
            "additionalProperties": false
        })
    );
    // A described property whose type's schema is empty.
    let mut out = String::new();
    property::<Value>(&mut out, "anything", &[]);
    assert_eq!(out, r#"{"description":"anything"}"#);
}
