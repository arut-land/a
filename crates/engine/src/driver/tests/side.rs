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
fn sides(log: &[Entry]) -> Vec<(CallId, u64, &str)> {
    log.iter()
        .filter_map(|e| match &e.body {
            Body::ModelCall {
                r#for: Some(of),
                purpose: Some(purpose),
                ..
            } => Some((CallId(e.seq), of.get(), purpose.as_str())),
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
    http.wait_sent(2);
    session.interrupt_now();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let (side, ..) = sides(&log)[0];
    let ended = |call: CallId| {
        log.iter().position(|e| {
            matches!(&e.body, Body::ModelResult { call: c, outcome: Outcome::Interrupted { .. } } if *c == call)
        })
    };
    let (side_end, call_end) = (ended(side), ended(CallId::new(2).unwrap()));
    assert!(side_end.is_some() && call_end.is_some(), "{log:#?}");
    assert!(side_end < call_end, "{log:#?}");
    assert_eq!(http.sent(), 2);
}

/// A request that overflowed is summarised aside, the summary stands for
/// what it summarises from then on, and the request is sent again from
/// the transcript the edit leaves; retry, installed after, is not asked.
#[test]
fn a_context_overflow_is_compacted_and_sent_again() {
    let http = Flaky::new([
        Reply::Status(413, &[]),
        Reply::Events(text_reply()),
        Reply::Events(text_reply()),
    ]);
    let registry = Registry::default()
        .with_in(ext::Tier::Pre, &ext_compact::Compact::default())
        .and_then(|r| r.with_in(ext::Tier::Post, &ext_retry::Retry::default()))
        .unwrap();
    let session = fixture(registry, &http).start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    assert_eq!(http.sent(), 3, "{log:#?}");
    assert_eq!(
        sides(&log).iter().map(|s| s.2).collect::<Vec<_>>(),
        ["compact"]
    );
    let edit = log.iter().find_map(|e| match &e.body {
        Body::Edit {
            target,
            upto,
            replace,
        } => Some((target.get(), upto.map(Seq::get), replace.clone())),
        _ => None,
    });
    let (target, upto, replace) = edit.expect("an edit");
    assert_eq!((target, upto), (1, Some(1)), "{log:#?}");
    let summary: String = replace.unwrap().parse().unwrap();
    assert!(summary.contains("crimson-harbor"), "{summary}");
    assert!(log.iter().any(|e| matches!(&e.body,
        Body::Decision { by, d: Verdict::Retry { attempt: 1, .. }, .. } if by == "compact")));
    assert!(matches!(
        log.last().map(|e| &e.body),
        Some(Body::ModelResult {
            outcome: Outcome::Done { .. },
            ..
        })
    ));
    // The model now reads the summary where the conversation was.
    let messages = transcript(&log, log.len() as u64 + 1, Arc::new(Registry::default()));
    assert!(matches!(&messages[0], llm::Message::User(u) if format!("{u:?}").contains("summary")));
}

/// An edit changes what the model sees from then on, and nothing else:
/// the transcript before it is as it was, and the log keeps every entry.
#[test]
fn an_edit_changes_only_what_the_model_sees_after_it() {
    let summary = Json::encode("the gist").unwrap();
    let entries = [
        entry(1, user("first")),
        entry(2, echo("first")),
        entry(
            3,
            Body::Edit {
                target: Seq::new(1).unwrap(),
                upto: Seq::new(2),
                replace: Some(summary),
            },
        ),
        entry(4, user("second")),
    ];
    let registry = || Arc::new(Registry::default());
    let before = transcript(&entries[..2], 3, registry());
    assert_eq!(
        before,
        [
            llm::Message::user("first"),
            llm::Message::assistant("first")
        ]
    );
    let after = transcript(&entries, 5, registry());
    assert_eq!(
        after,
        [llm::Message::user("the gist"), llm::Message::user("second")]
    );
    // A later edit of the same entries is what stands.
    let mut again = entries.to_vec();
    let newer = Json::encode("newer").unwrap();
    again.push(entry(
        5,
        Body::Edit {
            target: Seq::new(1).unwrap(),
            upto: Seq::new(2),
            replace: Some(newer),
        },
    ));
    let after = transcript(&again, 6, registry());
    assert_eq!(
        after,
        [llm::Message::user("newer"), llm::Message::user("second")]
    );
}

/// A session is titled by a side request beside its first model call: its
/// reply, logged as the side request's outcome, is the title clients
/// show; the conversation never hears of it, and no later call asks again.
#[test]
fn a_title_call_is_a_side_call() {
    let http = Flaky::new([
        Reply::Events(text_reply()),
        Reply::Events(text_reply()),
        Reply::Events(text_reply()),
    ]);
    let registry = Registry::default()
        .with_in(ext::Tier::Post, &ext_title::Title)
        .unwrap();
    let session = fixture(registry, &http).start(&[]);
    session.submit("hi").unwrap();
    until(&session, "titled", |log| {
        sides(log).first().is_some_and(|&(side, ..)| {
            log.iter()
                .any(|e| matches!(&e.body, Body::ModelResult { call, .. } if *call == side))
        })
    });
    session.submit("again").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let titles: Vec<_> = sides(&log).iter().map(|s| s.2).collect();
    assert_eq!(titles.first(), Some(&"title"), "{log:#?}");
    let view = SessionView::from_log(SessionId::new(), "/w".into(), &log);
    assert_eq!(
        view.side(ext_title::PURPOSE),
        Some("The harbor label is crimson-harbor.")
    );
    assert_eq!(titles.len(), 1, "titled once");
}

#[test]
fn side_failures_keep_classification_without_provider_content() {
    let secret = "private provider body, message and request id";
    for (kind, status, classified) in [
        (llm::Kind::RateLimit, Some(429), "rate_limit"),
        (llm::Kind::Auth, Some(401), "auth"),
        (llm::Kind::Transport, None, "transport"),
    ] {
        let session = SessionId::new();
        let mut error = llm::Error::new(kind, secret);
        error.status = status;
        error.body = Some(secret.into());
        error.request_id = Some(secret.into());
        assert!(sided(session, Some(Replied::Failed(error))).1.is_err());
        let records: Vec<_> = ext_logging::testing::pending().into_iter().filter(|fact| {
            matches!(fact, ext_logging::Fact::ProviderError { session: id, .. } if *id == session)
        }).collect();
        assert_eq!(records.len(), 1);
        let text = serde_json::to_string(&records[0]).unwrap();
        let record: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(record["provider_error"]["kind"], classified);
        assert_eq!(
            record["provider_error"]["status"],
            serde_json::to_value(status).unwrap()
        );
        assert_eq!(record["provider_error"]["retryable"], kind.is_retryable());
        assert!(!text.contains(secret));
    }
    let session = SessionId::new();
    assert!(sided(session, None).1.is_err());
    assert!(
        ext_logging::testing::pending()
            .iter()
            .any(|fact| matches!(fact,
        ext_logging::Fact::ProviderError { session: id, kind: ext_logging::Provider::Transport, .. }
        if *id == session))
    );
}
