//! `title`: a session is titled by its model, asked aside as its first
//! model call goes out. The title is the side request's reply, logged as
//! its outcome: clients read it from there (`SessionView::side(PURPOSE)`),
//! so no extension state holds it twice. A session-scoped request key
//! keeps a session from being titled again.

use ext::point::Observe;
use ext::{Budget, Configured, Extension, Reaction, Refused, Registrar, Side, Table};
use protocol::Body;

/// The purpose the side request is logged with: what a client asks the
/// view for.
pub const PURPOSE: &str = "title";

/// What the model is asked.
const TITLE: &str = "Name this conversation in at most six words, for a list of sessions: the task, not the tools. Answer with the name alone.";

/// The `title` extension.
pub struct Title;

impl Configured for Title {
    fn configure(_: &impl Table) -> Result<Self, Refused> {
        Ok(Self)
    }
}

impl Extension for Title {
    const NAME: &'static str = "title";
    const VERSION: u32 = 2;

    /// Reacts to the first model call of the session: a side request on
    /// its behalf, owned by the session, once the call's intent is
    /// committed.
    fn contribute(&self, r: &mut Registrar) {
        r.on::<Observe>(|cx, entry| {
            let first = matches!(
                entry.body,
                Body::ModelCall {
                    attempt: 0,
                    r#for: None,
                    ..
                }
            );
            if !first {
                return Reaction::NONE;
            }
            let Some(cx) = cx.cx() else {
                return Reaction::NONE;
            };
            let ask = [llm::Message::user(TITLE)].into_iter().collect();
            let mut side = Side::new::<Title>(
                PURPOSE,
                ext::ModelRole::Fast,
                ext::SideContext::BeforeOwner,
                ask,
            )
            .once_per_session();
            side.budget = Budget {
                max_tokens: std::num::NonZeroU32::new(32).unwrap(),
                ..Budget::default()
            };
            side.background().start(cx)
        });
    }
}
