//! `subagent`: the `task` tool, which hands a self-contained piece of work
//! to a child session and answers with what it settled with. The engine's
//! [`Sessions`] starts the child, owned by the call: it is interrupted with
//! the turn, survives a restart, and is heard from once.

use ext::{Cx, Extension, Registrar, Replay, Sessions, Text, Tool};
use serde::Deserialize;
use serde_json::Value;

/// The `subagent` extension.
pub struct Subagent;

impl Extension for Subagent {
    const NAME: &'static str = "subagent";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Task).text(Text::system("task", 30, "`task` hands a self-contained piece of work to a fresh session, which sees nothing of this conversation: say all it needs in `prompt`. It answers with what that session ended with.").for_tool(Task::NAME));
    }
}

/// `task`: runs a child session on a prompt.
pub struct Task;

#[derive(Deserialize)]
pub struct Prompt {
    pub prompt: String,
}

impl Tool for Task {
    type In = Prompt;
    type Out = String;

    const NAME: &'static str = "task";
    const DESCRIPTION: &'static str =
        "Runs a piece of work in a new session of its own and returns what it ended with.";
    /// Run again, it waits on the child it started, which is the call's.
    const REPLAY: Replay = Replay::Safe;

    fn schema() -> Value {
        serde_json::from_str(
            r#"{
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "The work, said in full." }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }"#,
        )
        .expect("a tool schema is valid JSON")
    }

    async fn call(&self, input: Prompt, cx: &Cx) -> Result<String, String> {
        let sessions = cx
            .service::<dyn Sessions>()
            .ok_or("this engine runs no child sessions")?;
        let outcome = sessions.child(cx, input.prompt).await;
        Ok(ext::child_result(&outcome))
    }

    fn to_model(text: &String) -> llm::Output {
        llm::Output::Text(text.clone())
    }
}
