//! `subagent`: the `task` tool, which hands a self-contained piece of work
//! to a child session and answers with what it settled with. Each tool is
//! an [`ext::Conversation`] task, owned by the call: it is interrupted with
//! the turn, survives a restart, and is heard from once.

use ext::{
    Concurrency, Configured, Conversation, Cx, Extension, Json, Refused, Registrar, Replay, Schema,
    Table, Task as Child, Text, Tool,
};
use serde::{Deserialize, Serialize};

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
        r.agent_kind::<Reviewer>()
            .fold::<ReviewPolicy>()
            .on::<ext::point::Permission>(|cx, _| {
                if cx.state(|policy: &ReviewPolicy| policy.0) {
                    protocol::Permission::Deny
                } else {
                    protocol::Permission::Allow
                }
            });
        r.provide::<dyn ext::Route>(std::sync::Arc::new(Self));
        r.tool(Task).tool(ForkTask).tool(BackgroundTask).text(Text::system("task", 30, "`task` hands a self-contained piece of work to a fresh session, which sees nothing of this conversation: say all it needs in `prompt`. It answers with what that session ended with.").for_tool(Task::NAME));
    }
}

/// `task`: runs a child session on a prompt.
pub struct Task;

#[derive(Deserialize, Schema)]
pub struct Prompt {
    /// The work, said in full.
    pub prompt: String,
    /// Optional agent kind; omission keeps the current kind.
    pub kind: Option<String>,
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
        awaited(child("task", input), cx).await
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
        awaited(child("fork_task", input).owner_context(), cx).await
    }
    fn to_model(output: &Json) -> llm::Output {
        Task::to_model(output)
    }
}

fn child(purpose: &str, input: &Prompt) -> Conversation<Child> {
    let child = Conversation::task::<Subagent>(purpose, &input.prompt);
    input.kind.iter().fold(child, Conversation::kind)
}

async fn awaited(child: Conversation<Child>, cx: &Cx) -> Result<String, String> {
    let outcome = child.ask(cx).await.map_err(|e| format!("{e:?}"))?;
    let shown = match &outcome {
        protocol::Outcome::Done {
            output: protocol::Payload::Blob(blob),
        } => read(cx, blob).await,
        _ => None,
    };
    Ok(ext::child_result(&outcome, shown.as_deref()))
}

/// What a child's result held as a blob shows the model (its text, kept
/// whole), read back on a storage worker.
async fn read(cx: &Cx, blob: &protocol::BlobRef) -> Option<String> {
    let rt = cx.service::<dyn rt::Runtime>()?;
    let (places, blob) = (cx.blobs.places(), blob.clone());
    rt::unblock(rt, move || places.read(&blob)).await
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
        let handle = Conversation::task::<Subagent>("background_task", &input.prompt)
            .detach(cx)
            .await
            .map_err(|e| format!("{e:?}"))?;
        for (index, message) in input.messages.iter().enumerate() {
            handle
                .send::<Subagent>(cx, &format!("message/{index}"), message.clone())
                .await
                .map_err(|e| format!("{e:?}"))?;
        }
        Ok(handle)
    }
}

/// Messages go along ownership edges only: to the parent, or a child.
impl ext::Route for Subagent {
    fn resolve(
        &self,
        from: ext::Topology<'_>,
        to: &ext::Recipient,
    ) -> Result<ext::Recipients, protocol::InboxRefusal> {
        let to = match to {
            ext::Recipient::Parent => from.parent,
            ext::Recipient::Conversation(handle) => Some(handle.session)
                .filter(|s| from.parent == Some(*s) || from.children.contains(s)),
            ext::Recipient::Named(_) => None,
        };
        to.map(|to| ext::Recipients::from_elem(to, 1))
            .ok_or(protocol::InboxRefusal::Unauthorized)
    }
}

/// Reviews supplied context without compaction or tool side effects.
pub struct Reviewer;
impl ext::AgentKind for Reviewer {
    const NAME: &'static str = "reviewer";
    const INHERIT: ext::Inheritance = ext::Inheritance::Except(&[ext_compact::Compact::NAME]);
    const INHERIT_SETTINGS: bool = false;
    fn settings(states: &mut ext::States) {
        states.set(ReviewPolicy(true));
    }
    fn make() -> Box<dyn ext::Agent> {
        Box::new(ext::Loop::default())
    }
}

#[derive(Default, Serialize, Deserialize)]
struct ReviewPolicy(bool);
impl ext::Fold for ReviewPolicy {
    const NAME: &'static str = "review-policy";
    const SETTINGS: bool = true;
    type Change = bool;
    fn apply(&mut self, value: &bool) {
        self.0 = *value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ext::AgentKind;
    use ext::host::Registry;
    use ext::point::{PointCx, View};

    struct Settings(std::sync::Mutex<ext::States>);
    impl View for Settings {
        fn session(&self) -> protocol::SessionId {
            protocol::SessionId::new()
        }
        fn workspace(&self) -> &std::path::Path {
            std::path::Path::new("/w")
        }
        fn model(&self) -> String {
            String::new()
        }
        fn states(&self, read: &mut dyn FnMut(&ext::States)) {
            read(&self.0.lock().unwrap());
        }
    }

    #[test]
    fn reviewer_policy_survives_its_settings_image_and_cannot_allow_tool_effects() {
        let root = Registry::default()
            .with(&ext_compact::Compact::default())
            .unwrap()
            .with(&Subagent)
            .unwrap();
        let child = root.select(Reviewer::INHERIT, None).unwrap();
        assert!(child.tools().get("compact").is_none());
        let mut states = child.states();
        child.kind_settings(Reviewer::NAME, &mut states);
        let image = Json::encode(&states.settings_image().unwrap()).unwrap();
        let mut restored = child.states();
        restored.restore_settings(&image.parse().unwrap()).unwrap();
        let view = Settings(std::sync::Mutex::new(restored));
        let denied = child
            .hooks()
            .gate(&PointCx::new(&view), "bash", &Json::null())
            .now()
            .unwrap();
        assert!(matches!(denied.gate, ext::Gate::Refuse(_)));
        let root_view = Settings(std::sync::Mutex::new(root.states()));
        let allowed = root
            .hooks()
            .gate(&PointCx::new(&root_view), "bash", &Json::null())
            .now()
            .unwrap();
        assert_eq!(allowed.gate, ext::Gate::Run);
    }
}
