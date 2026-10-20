//! Side model requests: what an extension asks a model aside, on behalf of
//! a call.

use super::retry::{Flaky, Reply, fixture, text_reply};
use super::*;

/// Recovers a rate-limited call after asking a model aside, as compaction
/// recovers an overflow: a `request_error` handler that answers later.
struct Aside;

impl Extension for Aside {
    const NAME: &'static str = "aside";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<point::RequestError>(|cx, failed| {
            if failed.error.kind != llm::Kind::RateLimit || failed.attempt > 0 {
                return Answer::Now(None);
            }
            let Some(cx) = cx.cx() else {
                return Answer::Now(None);
            };
            Answer::Later(ext::Later::new(async move {
                let models = cx.models()?;
                let side = ext::Side::new(
                    "test",
                    [llm::Message::user("Say hi.")].into_iter().collect(),
                );
                models.call(&cx, side).await.ok()?;
                Some(Recover::Resend)
            }))
        });
    }
}

/// The side calls in `log`, as `(call, for, purpose)`.
fn sides(log: &[Entry]) -> Vec<(u64, u64, &str)> {
    log.iter()
        .filter_map(|e| match &e.body {
            Body::ModelCall {
                r#for: Some(of),
                purpose: Some(purpose),
                ..
            } => Some((e.seq, of.get(), purpose.as_str())),
            _ => None,
        })
        .collect()
}

/// A side request is logged under its call as a model call of its own,
/// with its reply; neither is the conversation's, and the call goes on as
/// its handler said.
#[test]
fn a_side_call_is_logged_as_its_own_call() {
    let http = Flaky::new([
        Reply::Status(429, &[]),
        Reply::Events(text_reply()),
        Reply::Events(text_reply()),
    ]);
    let registry = Registry::default().with(&Aside).unwrap();
    let session = fixture(registry, &http).start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    assert_eq!(http.sent(), 3);
    let sides = sides(&log);
    assert_eq!(sides.len(), 1, "{log:#?}");
    let (side, of, purpose) = sides[0];
    assert_eq!((of, purpose), (2, "test"));
    let answered = log.iter().any(
        |e| matches!(&e.body, Body::ModelResult { call, outcome: Outcome::Done { .. } } if *call == side),
    );
    assert!(answered, "{log:#?}");
    // Resent once the side request answered, as the handler's decision.
    assert!(log.iter().any(|e| matches!(&e.body,
        Body::Decision { by, call: Some(c), d: Verdict::Retry { attempt: 1, .. } } if by == "aside" && c.get() == 2)));
    // The conversation the model sees has no word of it.
    let messages = transcript(&log, log.len() as u64 + 1, Arc::new(Registry::default()));
    let replies = messages
        .iter()
        .filter(|m| matches!(m, llm::Message::Assistant(_)))
        .count();
    assert_eq!(replies, 1, "{messages:#?}");
    let view = SessionView::from_log(SessionId::new(), "/w".into(), &log);
    assert!(view.items.iter().any(|i| matches!(i,
        protocol::view::Item::Reply { purpose: Some(p), .. } if p == "test")));
}

/// A side request ends with its call: an interrupt while it is out cancels
/// it, its end logged before its call's.
#[test]
fn a_side_call_dies_with_its_call() {
    let http = Flaky::new([Reply::Status(429, &[]), Reply::Hang(String::new())]);
    let registry = Registry::default().with(&Aside).unwrap();
    let session = fixture(registry, &http).start(&[]);
    session.submit("hi").unwrap();
    until(&session, "asked aside", |log| !sides(log).is_empty());
    session.interrupt_now();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let (side, ..) = sides(&log)[0];
    let ended = |call: u64| {
        log.iter().position(|e| {
            matches!(&e.body, Body::ModelResult { call: c, outcome: Outcome::Interrupted { .. } } if *c == call)
        })
    };
    let (side_end, call_end) = (ended(side), ended(2));
    assert!(side_end.is_some() && call_end.is_some(), "{log:#?}");
    assert!(side_end < call_end, "{log:#?}");
    assert_eq!(http.sent(), 2);
}
