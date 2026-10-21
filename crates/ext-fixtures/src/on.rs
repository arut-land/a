use std::time::Duration;

use ext::{
    Action, Actions, Agent, Concurrency, Cx, Extension, Input, LoopPoints, Registrar, Replay,
    Schema, Tool,
};
use protocol::Json;
use serde::{Deserialize, Serialize};

/// The `fixtures` extension.
pub struct Fixtures;

impl Extension for Fixtures {
    const NAME: &'static str = "fixtures";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn ext::SideProvider>(std::sync::Arc::new(Fixtures));
        r.tool(Sleep).agent(crate::ECHO, || Box::new(Echo));
    }
}

/// Echoes messages back. `$ <command>` runs it with `bash`,
/// `sleep <seconds>` calls the `sleep` fixture, `spawn <message>` hands
/// the message to a child session with `task`, and `par <n> <message>`
/// makes n of the call the message makes, so tool calls and links can be
/// exercised without a model.
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
        // `par <n> <message>`: n of the call the message makes, at once, as
        // a model reply with several tool calls asks for them.
        if let Some((n, text)) = (text.strip_prefix("par "))
            .and_then(|rest| rest.split_once(' '))
            .and_then(|(n, text)| Some((n.parse::<usize>().ok()?, text)))
            && let Some((tool, input)) = call(text)
        {
            return (0..n)
                .map(|i| Action::Call {
                    tool: tool.to_owned(),
                    input: input.clone(),
                    id: Some(format!("p{i}")),
                })
                .collect();
        }
        let Some((tool, input)) = call(&text) else {
            return Actions::from_elem(Action::Say(text), 1);
        };
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

/// The tool and input a message calls, if it calls one.
fn call(text: &str) -> Option<(&'static str, Json)> {
    let (tool, input) = if let Some(prompt) = text.strip_prefix("spawn ") {
        ("task", Call::Task { prompt })
    } else if let Some(command) = text.strip_prefix("$ ") {
        ("bash", Call::Bash { command })
    } else if let Some(Ok(seconds)) = text.strip_prefix("sleep ").map(str::parse::<u64>) {
        (Sleep::NAME, Call::Sleep { seconds })
    } else {
        return None;
    };
    Some((tool, Json::encode(&input).ok()?))
}

/// A call's input, as each tool reads it.
#[derive(Serialize)]
#[serde(untagged)]
enum Call<'a> {
    Task { prompt: &'a str },
    Bash { command: &'a str },
    Sleep { seconds: u64 },
}

/// Waits: a safe tool, so tests can watch a call that recovery reruns.
pub struct Sleep;

#[derive(Deserialize, Schema)]
pub struct Seconds {
    #[schema(min = 0)]
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
    const CONCURRENCY: Concurrency = Concurrency::Shared;

    async fn call(&self, input: &Seconds, cx: &Cx) -> Result<Slept, String> {
        let rt = cx
            .service::<dyn rt::Runtime>()
            .ok_or("the engine provides no runtime")?;
        rt.sleep(Duration::from_secs(input.seconds)).await;
        Ok(Slept {
            slept: input.seconds,
        })
    }
}

impl ext::Provides<dyn ext::SideProvider> for Fixtures {}

impl ext::SideProvider for Fixtures {
    fn reply<'a>(
        &'a self,
        side: &'a ext::Side,
        _: &'a llm::Messages,
    ) -> ext::BoxFuture<'a, Result<llm::Response, llm::Error>> {
        Box::pin(async move {
            Ok(llm::Response {
                message: llm::Assistant {
                    content: vec![llm::AssistantPart::Text(llm::Text::new(format!(
                        "fixture {}",
                        side.purpose
                    )))],
                    origin: None,
                },
                finish: llm::Finish::new(llm::Reason::Stop, None),
                usage: llm::Usage::default(),
                id: None,
                model: None,
                meta: llm::Meta::new(),
            })
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
                output: Json::encode("hi").unwrap(),
            },
        };
        assert_eq!(
            &Echo.step(done, &ext::Defaults)[..],
            [Action::Say("The child session finished:\nhi".into())]
        );
    }
}
