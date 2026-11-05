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
        .max_tokens(const { std::num::NonZeroU32::new(32).unwrap() })
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
        let reply = title(prompt)
            .ask(cx)
            .await
            .map_err(|error| format!("Title was not changed: {error}"))?;
        let text = reply.message.text();
        let text = text.trim().trim_matches('"').trim();
        if text.is_empty() {
            return Err("Title was not changed: the model returned an empty title.".into());
        }
        Ok(text.to_owned())
    }
}
