//! `subagent`: the `task` tool, which hands a self-contained piece of work
//! to a child session and answers with what it settled with. The engine's
//! [`Sessions`] starts the child, owned by the call: it is interrupted with
//! the turn, survives a restart, and is heard from once.

use ext::{
    Concurrency, Configured, Cx, Extension, Json, Refused, Registrar, Replay, Schema, Sessions,
    Table, Text, Tool,
};
use serde::Deserialize;

/// The `subagent` extension.
pub struct Subagent;

impl Configured for Subagent {
    fn configure(_: &impl Table) -> Result<Self, Refused> {
        Ok(Self)
    }
}

impl Extension for Subagent {
    const NAME: &'static str = "subagent";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn ext::InboxAccess>(std::sync::Arc::new(Self));
        r.tool(Task).tool(ForkTask).tool(BackgroundTask).text(Text::system("task", 30, "`task` hands a self-contained piece of work to a fresh session, which sees nothing of this conversation: say all it needs in `prompt`. It answers with what that session ended with.").for_tool(Task::NAME));
    }
}

/// `task`: runs a child session on a prompt.
pub struct Task;

#[derive(Deserialize, Schema)]
pub struct Prompt {
    /// The work, said in full.
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
    /// Its work is the child's, in a session of its own.
    const CONCURRENCY: Concurrency = Concurrency::Shared;

    async fn call(&self, input: &Prompt, cx: &Cx) -> Result<String, String> {
        let sessions = cx
            .service::<dyn Sessions>()
            .ok_or("this engine runs no child sessions")?;
        let outcome = sessions
            .spawn(
                cx,
                ext::Spawn::new::<Subagent>(
                    "task",
                    input.prompt.clone(),
                    sessions.runner(),
                    ext::DepthBudget::new(8),
                )
                .awaited(),
            )
            .await
            .map_err(|e| format!("{e:?}"))?;
        Ok(ext::child_result(&outcome))
    }

    /// The child's text, as text rather than a JSON string.
    fn to_model(output: &Json) -> llm::Output {
        output
            .parse()
            .map_or_else(|_| llm::Output::Json(output.clone()), llm::Output::Text)
    }
}

/// A current-context task can be admitted while its owning turn is open.
pub struct ForkTask;
impl Tool for ForkTask {
    type In = Prompt;
    type Out = String;
    const NAME: &'static str = "fork_task";
    const DESCRIPTION: &'static str =
        "Runs work in a child with this conversation's current context.";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: Concurrency = Concurrency::Shared;
    async fn call(&self, input: &Prompt, cx: &Cx) -> Result<String, String> {
        let sessions = cx
            .service::<dyn Sessions>()
            .ok_or("this engine runs no child sessions")?;
        let mut spawn = ext::Spawn::new::<Subagent>(
            "fork_task",
            input.prompt.clone(),
            sessions.runner(),
            ext::DepthBudget::new(8),
        );
        spawn.context = ext::ContextSource::Prefix {
            session: cx.session,
            seq: cx.call.0,
        };
        let outcome = sessions
            .spawn(cx, spawn.awaited())
            .await
            .map_err(|e| format!("{e:?}"))?;
        Ok(ext::child_result(&outcome))
    }
    fn to_model(output: &Json) -> llm::Output {
        Task::to_model(output)
    }
}

/// Detached work keeps running after this call returns its handle.
pub struct BackgroundTask;
#[derive(Deserialize, Schema)]
pub struct BackgroundPrompt {
    pub prompt: String,
    /// Messages enqueued after admission, while the child's initial work runs.
    pub messages: Vec<String>,
}
impl Tool for BackgroundTask {
    type In = BackgroundPrompt;
    type Out = ext::ConversationHandle;
    const NAME: &'static str = "background_task";
    const DESCRIPTION: &'static str =
        "Starts owned background work and sends it messages; its result arrives later.";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: Concurrency = Concurrency::Shared;
    async fn call(&self, input: &BackgroundPrompt, cx: &Cx) -> Result<Self::Out, String> {
        let sessions = cx
            .service::<dyn Sessions>()
            .ok_or("this engine runs no child sessions")?;
        let spawn = ext::Spawn::new::<Subagent>(
            "background_task",
            input.prompt.clone(),
            sessions.runner(),
            ext::DepthBudget::new(8),
        );
        let handle = sessions
            .detach(cx, spawn.detached())
            .await
            .map_err(|e| format!("{e:?}"))?;
        for (index, message) in input.messages.iter().enumerate() {
            sessions
                .send(
                    cx,
                    ext::SendMessage::new::<Subagent>(
                        &format!("message/{index}"),
                        handle,
                        message.clone(),
                    ),
                )
                .await
                .map_err(|e| format!("{e:?}"))?;
        }
        Ok(handle)
    }
}

impl ext::InboxAccess for Subagent {
    fn permits(
        &self,
        destination: protocol::SessionId,
        parent: Option<protocol::SessionId>,
        children: &[protocol::SessionId],
    ) -> bool {
        parent == Some(destination) || children.contains(&destination)
    }
}
