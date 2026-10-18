//! The tools the built-in extensions offer: `bash` and `edit`, and a
//! `sleep` for tests.

pub mod edit;
mod output;
pub mod shell;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Cx, Replay, Tool};

pub use edit::Edit;
pub use shell::Bash;

/// A schema written as JSON text: smaller in the binary than one built.
fn schema(text: &str) -> Value {
    serde_json::from_str(text).expect("a tool schema is valid JSON")
}

/// Waits: a safe tool, so tests can watch a call that recovery reruns. Only
/// the `fixtures` extension offers it.
pub struct Sleep;

#[derive(Deserialize)]
pub struct Seconds {
    pub seconds: u64,
}

#[derive(Serialize, Deserialize)]
pub struct Slept {
    pub slept: u64,
}

impl Tool for Sleep {
    type In = Seconds;
    type Out = Slept;

    const NAME: &'static str = "sleep";
    const DESCRIPTION: &'static str = "Waits for a number of seconds.";
    const REPLAY: Replay = Replay::Safe;

    fn schema() -> Value {
        schema(
            r#"{
                "type": "object",
                "properties": { "seconds": { "type": "integer", "minimum": 0 } },
                "required": ["seconds"],
                "additionalProperties": false
            }"#,
        )
    }

    async fn call(&self, input: Seconds, cx: &Cx) -> Result<Slept, String> {
        cx.rt.sleep(Duration::from_secs(input.seconds)).await;
        Ok(Slept {
            slept: input.seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{Fixtures, Policy, builtins};

    #[test]
    fn schemas_describe_objects_that_name_their_required_inputs() {
        let registry = builtins(&Policy::default())
            .and_then(|r| r.with(&Fixtures))
            .unwrap();
        for spec in registry.tools().specs() {
            let schema = &spec.schema;
            assert_eq!(schema["type"], "object", "{}", spec.name);
            assert_ne!(spec.description, "");
            for required in schema["required"].as_array().unwrap() {
                let name = required.as_str().unwrap();
                assert!(
                    schema["properties"][name].is_object(),
                    "{}: {name}",
                    spec.name
                );
            }
        }
    }
}
