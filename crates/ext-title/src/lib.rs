//! `title`: a session is titled by its model, asked aside as its first
//! model call goes out. The title is the side request's reply, logged as
//! its outcome: clients read it from there (`SessionView::side(PURPOSE)`),
//! so no extension state holds it twice. The `titled` fold keeps a
//! session from being titled again.

use ext::point::Observe;
use ext::{Budget, Configured, Extension, Fold, Reaction, Refused, Registrar, Side, Table};
use protocol::Body;

/// The purpose the side request is logged with: what a client asks the
/// view for.
pub const PURPOSE: &str = "title";

/// What the model is asked.
const TITLE: &str = "Name this conversation in at most six words, for a list of sessions: the task, not the tools. Answer with the name alone.";

/// The `title` extension.
pub struct Title;

/// Whether the session was titled, or is being.
#[derive(Debug, Default)]
pub struct Titled(pub bool);

impl Fold for Titled {
    const NAME: &'static str = "titled";
    type Change = bool;

    fn apply(&mut self, titled: &bool) {
        self.0 = *titled;
    }
}

impl Configured for Title {
    fn configure(_: &impl Table) -> Result<Self, Refused> {
        Ok(Self)
    }
}

impl Extension for Title {
    const NAME: &'static str = "title";
    const VERSION: u32 = 1;

    /// Reacts to the first model call of the session: a side request on
    /// its behalf, owned by the session, once the call's intent is
    /// committed.
    fn contribute(&self, r: &mut Registrar) {
        r.fold::<Titled>().on::<Observe>(|cx, entry| {
            let first = matches!(
                entry.body,
                Body::ModelCall {
                    attempt: 0,
                    r#for: None,
                    ..
                }
            );
            if !first || cx.state(|t: &Titled| t.0) {
                return Reaction::NONE;
            }
            let Some(cx) = cx.cx() else {
                return Reaction::NONE;
            };
            Reaction::later(async move {
                // Marked and asked at once: the mark goes first, and the
                // request goes out while the call it is asked for is open.
                let marked = cx.update::<Titled>(&true);
                if let Some(models) = cx.models() {
                    let ask = [llm::Message::user(TITLE)].into_iter().collect();
                    let mut side = Side::new(PURPOSE, ask).after_the_conversation();
                    side.budget = Budget {
                        max_tokens: 32,
                        ..Budget::default()
                    };
                    // Its reply is logged as the side request's outcome.
                    let _ = models.call(&cx, side).await;
                }
                marked.await;
            })
        });
    }
}
