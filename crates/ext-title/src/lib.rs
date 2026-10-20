//! `title`: a session is titled by its model, asked aside as its first
//! model call goes out. The title is the side request's reply, logged as
//! its outcome: clients read it from there (`SessionView::title`), so no
//! extension state holds it twice. The `titled` fold keeps a session from
//! being titled again.

use ext::point::Step;
use ext::{Budget, Extension, Fold, Registrar, Rewrite, Side};

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

impl Extension for Title {
    const NAME: &'static str = "title";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.fold::<Titled>().on::<Step>(|cx, _| {
            if cx.state(|t: &Titled| t.0) {
                return Rewrite::Pass;
            }
            if let Some(cx) = cx.cx()
                && let Some(rt) = cx.service::<dyn rt::Runtime>()
            {
                cx.update::<Titled>(true);
                let cx = cx.clone();
                rt.spawn(Box::pin(async move {
                    let Some(models) = cx.models() else { return };
                    let ask = [llm::Message::user(TITLE)].into_iter().collect();
                    let mut side = Side::new("title", ask).after_the_conversation();
                    side.budget = Budget {
                        max_tokens: 32,
                        ..Budget::default()
                    };
                    // Its reply is logged as the side request's outcome.
                    let _ = models.call(&cx, side).await;
                }));
            }
            Rewrite::Pass
        });
    }
}
