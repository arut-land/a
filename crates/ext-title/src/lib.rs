//! Session metadata lives in a persistent model-only conversation. Explicit
//! title requests append exchanges without putting them in the main transcript.

use ext::point::Observe;
use ext::{
    Args, Budget, Configured, Cx, Extension, ModelTurn, Reaction, Refused, Registrar, Replay,
    Table, Tool, TurnKey,
};
use protocol::{Audience, Body};

pub const PURPOSE: &str = "title";
const TITLE: &str = "Name this conversation in at most six words, for a list of sessions: the task, not the tools. Answer with the name alone.";

pub struct Title;

impl ext::ModelOnlyKind for Title {
    const CONTEXT: ext::ContextPolicy = ext::ContextPolicy {
        exchanges: 2,
        bytes: 8192,
    };
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
            turn(TurnKey("initial".into()), TITLE.into())
                .background()
                .start(cx)
        });
    }
}

fn turn(key: TurnKey, prompt: String) -> ModelTurn {
    let mut turn = ModelTurn::new::<Title>(
        ext::ThreadKey::new::<Title>(PURPOSE, ext::OwnerScope::Session),
        key,
        ext::ModelRole::Fast,
        ext::SideContext::BeforeOwner,
        [llm::Message::user(prompt)].into_iter().collect(),
    );
    turn.budget = Budget {
        max_tokens: std::num::NonZeroU32::new(32).unwrap(),
        ..Budget::default()
    };
    turn
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
        cx.models()
            .ok_or("this engine has no model service")?
            .call(cx, turn(TurnKey(cx.call.get().to_string()), prompt))
            .await
            .map(|reply| reply.message.text())
            .map_err(|error| error.to_string())
    }
}
