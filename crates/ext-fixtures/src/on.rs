use std::time::Duration;

use ext::{Action, Actions, Agent, Cx, Extension, Input, LoopPoints, Registrar, Replay, Tool};
use protocol::Json;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The `fixtures` extension.
pub struct Fixtures;

impl Extension for Fixtures {
    const NAME: &'static str = "fixtures";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Sleep).agent(crate::ECHO, || Box::new(Echo));
    }
}

/// Echoes messages back. `$ <command>` runs it with `bash`,
/// `sleep <seconds>` calls the `sleep` fixture and `spawn <message>` hands
/// the message to a child session with `task`, so tool calls and links can
/// be exercised without a model.
pub struct Echo;

impl Agent for Echo {
    fn step(&mut self, input: Input, _: &dyn LoopPoints) -> Actions {
        let text = match input {
            Input::User { text } => text,
            Input::ChildDone { outcome, .. } => {
                let text = ext::child_result(&outcome);
                return Actions::from_elem(Action::Say(text), 1);
            }
            Input::ToolDone { .. } | Input::ModelDone { .. } => return Actions::new(),
        };
        let (tool, input) = if let Some(prompt) = text.strip_prefix("spawn ") {
            ("task", json!({ "prompt": prompt }))
        } else if let Some(command) = text.strip_prefix("$ ") {
            ("bash", json!({ "command": command }))
        } else if let Some(Ok(seconds)) = text.strip_prefix("sleep ").map(str::parse::<u64>) {
            (Sleep::NAME, json!({ "seconds": seconds }))
        } else {
            return Actions::from_elem(Action::Say(text), 1);
        };
        let input = Json::new(&input);
        Actions::from_elem(
            Action::Call {
                tool: tool.to_owned(),
                input,
                id: None,
            },
            1,
        )
    }
}

/// Waits: a safe tool, so tests can watch a call that recovery reruns.
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
        serde_json::from_str(
            r#"{
                "type": "object",
                "properties": { "seconds": { "type": "integer", "minimum": 0 } },
                "required": ["seconds"],
                "additionalProperties": false
            }"#,
        )
        .expect("a tool schema is valid JSON")
    }

    async fn call(&self, input: Seconds, cx: &Cx) -> Result<Slept, String> {
        let rt = cx
            .service::<dyn rt::Runtime>()
            .ok_or("the engine provides no runtime")?;
        rt.sleep(Duration::from_secs(input.seconds)).await;
        Ok(Slept {
            slept: input.seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    use protocol::Outcome;

    use super::*;

    fn user(text: &str) -> Input {
        Input::User { text: text.into() }
    }

    fn tool_named(actions: &[Action]) -> Option<&str> {
        match actions {
            [Action::Call { tool, .. }] => Some(tool),
            _ => None,
        }
    }

    #[test]
    fn echo_routes_messages_and_tools() {
        let mut echo = Echo;
        assert!(
            matches!(&echo.step(user("hi"), &ext::Defaults)[..], [Action::Say(text)] if text == "hi")
        );
        assert_eq!(
            tool_named(&echo.step(user("$ ls"), &ext::Defaults)),
            Some("bash")
        );
        assert_eq!(
            tool_named(&echo.step(user("sleep 2"), &ext::Defaults)),
            Some("sleep")
        );
        assert!(matches!(
            &echo.step(user("sleep x"), &ext::Defaults)[..],
            [Action::Say(_)]
        ));
        assert_eq!(
            tool_named(&echo.step(user("spawn sleep 1"), &ext::Defaults)),
            Some("task")
        );
    }

    #[test]
    fn echo_hears_a_childs_result() {
        let done = Input::ChildDone {
            watch: protocol::Seq::new(3).unwrap(),
            outcome: Outcome::Done {
                output: Value::from("hi").into(),
            },
        };
        assert_eq!(
            &Echo.step(done, &ext::Defaults)[..],
            [Action::Say("The child session finished:\nhi".into())]
        );
    }
}
