//! `compact`: a conversation grown too large for its model is summarised,
//! by the model, and what the model sees from then on is the summary in
//! place of what it summarises: an `edit`, so the log keeps every word.
//!
//! It owns the context overflow (`llm::Kind::ContextOverflow`) at the
//! `request_error` point, and answers nothing else: a request that
//! overflowed is summarised aside, the edit committed, and the request
//! sent again from the transcript as the edit leaves it. A call is
//! compacted once: overflowing again, it fails. With a `threshold`, the
//! `step` point fails a request estimated past it as an overflow before
//! it is sent, so it is compacted at no cost. `/compact` compacts now.
//!
//! What it summarises is the conversation the call's model sees, sent as
//! it is, so the provider's cache of it serves the summary.

use ext::point::{RequestError, Step};
use ext::{
    Answer, Args, Configured, Cx, Extension, Fold, Later, PointCx, Recover, Refused, Registrar,
    Rewrite, Side, Table, Tool,
};
use protocol::{Audience, CallId, Seq};
use serde::{Deserialize, Serialize};

/// What the model is asked to write.
const SUMMARY: &str = "The conversation above no longer fits the model's context. Write a summary of it to continue from: the user's goal and every request they made (the latest word for word), what was done and found (files, commands, results, decisions), and what is left to do. Leave out nothing a continuation needs; nothing else.";

/// The `compact` extension.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compact {
    /// The estimated tokens past which a request is compacted before it is
    /// sent; with none, only when the provider says it overflowed.
    pub threshold: Option<u64>,
}

/// The calls compacted, which are not compacted again.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Compacted(pub Vec<u64>);

impl Fold for Compacted {
    const NAME: &'static str = "compacted";
    type Change = u64;

    fn apply(&mut self, call: &u64) {
        self.0.push(*call);
    }
}

impl Compact {
    /// Whether a failure is one this compacts: an overflow of a call not
    /// compacted yet.
    fn owns(cx: &PointCx<'_>, error: &llm::Error) -> Option<CallId> {
        let call = cx.call?;
        let done = cx.state(|c: &Compacted| c.0.contains(&call.get()));
        (error.kind == llm::Kind::ContextOverflow && !done).then_some(call)
    }
}

impl Configured for Compact {
    fn configure(table: &impl Table) -> Result<Self, Refused> {
        Ok(Self {
            threshold: table.whole("threshold")?,
        })
    }
}

impl Extension for Compact {
    const NAME: &'static str = "compact";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let threshold = self.threshold;
        r.fold::<Compacted>()
            .tool(CompactNow)
            .on::<RequestError>(|cx, failed| {
                let Some(call) = Compact::owns(cx, failed.error) else {
                    return Answer::Now(None);
                };
                let cx = cx.cx();
                Answer::Later(Later::new(async move {
                    compact(&cx?, call).await.ok()?;
                    Some(Recover::Resend)
                }))
            })
            .on::<Step>(move |cx, request| match threshold {
                Some(most)
                    if estimate(request) > most
                        && cx.call.is_some_and(|call| {
                            !cx.state(|c: &Compacted| c.0.contains(&call.get()))
                        }) =>
                {
                    let error = format!(
                        "the request is about {} tokens, past the {most} to compact at",
                        estimate(request)
                    );
                    Rewrite::Stop(Err(llm::Error::new(llm::Kind::ContextOverflow, error)))
                }
                _ => Rewrite::Pass,
            });
    }
}

/// About how many tokens `request`'s messages are: a quarter of their
/// text, a JSON output's as a few hundred, and media as a thousand.
fn estimate(request: &llm::Request) -> u64 {
    use llm::{Message, Output, UserPart};
    let parts = |parts: &[UserPart]| -> usize {
        (parts.iter())
            .map(|p| match p {
                UserPart::Text(t) => t.text.len(),
                UserPart::Media(_) => 4000,
            })
            .sum()
    };
    let bytes: usize = (request.messages.iter())
        .map(|m| match m {
            Message::System(s) => s.text().len(),
            Message::User(u) => parts(&u.content),
            Message::Assistant(a) => a.text().len(),
            Message::Tool(t) => (t.results.iter())
                .map(|r| match &r.output {
                    Output::Text(text) => text.len(),
                    Output::Json(_) => 1000,
                    Output::Content(content) => parts(content),
                })
                .sum(),
        })
        .sum();
    u64::try_from(bytes / 4).unwrap_or(u64::MAX)
}

/// Summarises the conversation before call `call` and has the model see
/// the summary in its place from now on.
async fn compact(cx: &Cx, call: CallId) -> Result<String, String> {
    let models = cx.models().ok_or("this engine asks no model aside")?;
    let ask = [llm::Message::user(SUMMARY)].into_iter().collect();
    let side = Side::new::<Compact>(
        "compact",
        ext::ModelRole::Session,
        ext::SideContext::BeforeOwner,
        ask,
    );
    let reply = models.call(cx, side).await.map_err(|e| e.to_string())?;
    let summary = reply.message.text();
    if summary.trim().is_empty() {
        return Err("the model wrote no summary".into());
    }
    let (first, last) = (Seq::new(1), Seq::new(call.get() - 1));
    let (Some(first), Some(last)) = (first, last) else {
        return Err("nothing to compact".into());
    };
    let text = format!("<summary of the conversation so far>\n{summary}\n</summary>");
    cx.edit(first, Some(last), Some(&text));
    // A crash may repeat an edit; it must never retain only its completion mark.
    cx.update::<Compacted>(&call.get())
        .await
        .map_err(|e| e.to_string())?;
    Ok(summary)
}

/// `/compact`: summarises the conversation now, for the model to go on
/// from.
pub struct CompactNow;

impl Tool for CompactNow {
    type In = Args;
    type Out = String;

    const NAME: &'static str = "compact";
    const DESCRIPTION: &'static str = "summarise the conversation for the model to go on from";
    const AUDIENCE: Audience = Audience::User;

    async fn call(&self, _: &Args, cx: &Cx) -> Result<String, String> {
        compact(cx, cx.call).await
    }
}

#[cfg(test)]
mod tests;
