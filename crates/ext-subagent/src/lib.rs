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
        r.on::<ext::point::Step>(|cx, request| {
            let Some(delegation) = cx.delegation() else { return ext::Rewrite::Pass; };
            let assigned = delegation.prompt.map_or_else(String::new, |prompt| format!(
                "You are a subagent. Your assigned prompt is your current task; inherited conversation is reference context. Do this work yourself and follow subsequent user instructions in this conversation. Assigned prompt: {prompt}\n"
            ));
            let mut request = request.clone();
            request.messages.push(llm::Message::user(format!(
                "<system-reminder>\n{assigned}Delegation allowance: {} descendant sessions remain. Each child consumes one slot and receives half the remaining slots; those slots leave your allowance. Delegate only distinct supporting work; retain your own contribution and synthesize the results.\n</system-reminder>",
                delegation.allowance.remaining()
            )));
            ext::Rewrite::Replace(request)
        });
        r.provide::<dyn ext::Route>(std::sync::Arc::new(Self));
        r.tool(Task).tool(BackgroundTask).text(Text::system("task", 30, "`task` delegates a distinct supporting piece of work. Keep your own contribution and synthesize the results; do not hand off your whole assignment. By default a fresh child sees only `prompt`; set `fork` to true to include current context as reference. The prompt is always the child's own task. Fresh and forked children support the same agent kinds and options. Each child receives a share of the remaining delegation allowance.").for_tool(Task::NAME));
    }
}

/// `task`: runs a child session on a prompt.
pub struct Task;

#[derive(Deserialize, Schema)]
pub struct Prompt {
    /// The distinct supporting work, said in full.
    pub prompt: String,
    /// Optional agent kind; omission keeps the current kind.
    pub kind: Option<String>,
    /// Include this conversation as reference context; defaults to false.
    #[serde(default)]
    pub fork: bool,
}

impl Tool for Task {
    type In = Prompt;
    type Out = String;

    const NAME: &'static str = "task";
    const DESCRIPTION: &'static str =
        "Runs supporting work in a child; fork includes current context. Returns its result.";
    /// Run again, it waits on the child it started, which is the call's.
    const REPLAY: Replay = Replay::Safe;
    /// Its work is the child's, in a session of its own.
    const CONCURRENCY: Concurrency = Concurrency::Shared;

    async fn call(&self, input: &Prompt, cx: &Cx) -> Result<String, String> {
        awaited(child(input), cx).await
    }

    /// The child's text, as text rather than a JSON string.
    fn to_model(output: &Json) -> llm::Output {
        output
            .parse()
            .map_or_else(|_| llm::Output::Json(output.clone()), llm::Output::Text)
    }
}

fn child(input: &Prompt) -> Conversation<Child> {
    let child = input.kind.iter().fold(
        Conversation::task::<Subagent>("task", &input.prompt),
        Conversation::kind,
    );
    if input.fork {
        child.owner_context()
    } else {
        child
    }
}

async fn awaited(child: Conversation<Child>, cx: &Cx) -> Result<String, String> {
    let outcome = child.ask(cx).await.map_err(|e| e.to_string())?;
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
            .map_err(|e| e.to_string())?;
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
