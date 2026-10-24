//! Session metadata lives in a persistent model-only conversation. Explicit
//! title requests append exchanges without putting them in the main transcript.

use ext::point::Observe;
use ext::{
    Args, Configured, Conversation, Cx, Extension, Reaction, Refused, Registrar, Replay, Table,
    Tool, Turn,
};
use protocol::{Audience, Body};

pub const PURPOSE: &str = "title";
const TITLE: &str = "Name this conversation in at most six words, for a list of sessions: the task, not the tools. Answer with the name alone.";

pub struct Title;

/// A title is a few fast words about the recent conversation.
fn title(prompt: impl Into<String>) -> Conversation<Turn> {
    Conversation::turn::<Title>(PURPOSE, prompt)
        .fast()
        .owner_context()
        .bounds(2, 8192)
        .max_tokens(32)
}

impl Configured for Title {
    fn configure(_: &impl Table) -> Result<Self, Refused> {
        Ok(Self)
    }
}

impl Extension for Title {
    const NAME: &'static str = "title";
    const VERSION: u32 = 3;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Rename).on::<Observe>(|cx, entry| {
            if !matches!(
                entry.body,
                Body::ModelCall {
                    attempt: 0,
                    r#for: None,
                    ..
                }
            ) {
                return Reaction::NONE;
            }
            let Some(cx) = cx.cx() else {
                return Reaction::NONE;
            };
            title(TITLE).key("initial").detach().start(cx)
        });
    }
}

pub struct Rename;
impl Tool for Rename {
    type In = Args;
    type Out = String;
    const NAME: &'static str = "title";
    const DESCRIPTION: &'static str = "regenerate the title, optionally with naming instructions";
    const AUDIENCE: Audience = Audience::User;
    const REPLAY: Replay = Replay::Safe;

    async fn call(&self, args: &Args, cx: &Cx) -> Result<String, String> {
        let prompt = if args.args.trim().is_empty() {
            TITLE.into()
        } else {
            format!("{TITLE}\nNaming instructions: {}", args.args)
        };
        title(prompt)
            .ask(cx)
            .await
            .map(|reply| reply.message.text())
            .map_err(|error| error.to_string())
    }
}
