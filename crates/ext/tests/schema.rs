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
    let mut out = String::new();
    property::<Value>(&mut out, "anything", &[]);
    assert_eq!(out, r#"{"description":"anything"}"#);
}

#[test]
fn advertised_properties_match_deserialization() {
    #[derive(Deserialize, Schema)]
    #[serde(deny_unknown_fields)]
    struct Input {
        #[serde(rename = "type", skip_serializing)]
        r#kind: String,
        #[serde(default = "default_count", skip_serializing_if = "Option::is_none")]
        count: Option<u64>,
        #[serde(default)]
        enabled: bool,
    }
    fn default_count() -> Option<u64> {
        Some(3)
    }
    let schema: Value = serde_json::from_str(&of::<Input>()).unwrap();
    assert_eq!(schema["required"], json!(["type"]));
    let values = json!({"type": "call", "count": 8, "enabled": true});
    let properties = schema["properties"].as_object().unwrap();
    assert_eq!(properties.len(), values.as_object().unwrap().len());
    for name in properties.keys() {
        assert!(values.get(name).is_some(), "{name}");
    }
    let decoded: Input = serde_json::from_value(values).unwrap();
    assert_eq!(
        (decoded.kind.as_str(), decoded.count, decoded.enabled),
        ("call", Some(8), true)
    );
    let defaulted: Input = serde_json::from_value(json!({"type": "call"})).unwrap();
    assert_eq!((defaulted.count, defaulted.enabled), (Some(3), false));
    assert!(serde_json::from_value::<Input>(json!({"count": 8})).is_err());
    assert!(serde_json::from_value::<Input>(json!({"kind": "call"})).is_err());
    assert!(serde_json::from_value::<Input>(json!({"type": "call", "extra": 1})).is_err());
}

#[test]
fn unsupported_serde_shape_changes_fail_to_compile() {
    use std::{fs, path::Path, process::Command};

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let home = std::env::temp_dir().join(format!("a-schema-ui-{}", std::process::id()));
    fs::create_dir_all(home.join("src")).unwrap();
    fs::write(home.join("Cargo.toml"), format!(
        "[package]\nname = \"schema-ui\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n[dependencies]\next = {{ path = {:?} }}\nserde = {{ version = \"1\", features = [\"derive\"] }}\n",
        manifest,
    )).unwrap();
    fs::copy(
        manifest.join("tests/schema/unsupported.rs"),
        home.join("src/lib.rs"),
    )
    .unwrap();
    let output = Command::new(env!("CARGO"))
        .args(["check", "--offline", "--color=never", "--manifest-path"])
        .arg(home.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", home.join("target"))
        .env("CARGO_BUILD_JOBS", "2")
        .output();
    fs::remove_dir_all(&home).unwrap();
    let output = output.unwrap();
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(
        !output.status.success(),
        "unsupported serde attributes compiled"
    );
    assert_eq!(
        errors
            .matches("error: unsupported serde container attribute in Schema")
            .count(),
        6,
        "{errors}"
    );
    assert_eq!(
        errors
            .matches("error: unsupported serde field attribute in Schema")
            .count(),
        7,
        "{errors}"
    );
    assert!(
        errors.contains("duplicate deserialization property in Schema"),
        "{errors}"
    );
}
