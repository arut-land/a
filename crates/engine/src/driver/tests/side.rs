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
                let side = ext::ModelTurn::new::<ext::ModelOnly>(
                    ext::ThreadKey::new::<Aside>("test", ext::OwnerScope::Call(cx.call)),
                    ext::TurnKey("initial".into()),
                    ext::ModelRole::Session,
                    ext::SideContext::None,
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
fn side_threads_never_enter_the_main_transcript() {
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
        |e| matches!(&e.body, Body::ModelResult { r#for: Some(_), failure: None, call, outcome: Outcome::Done { .. } } if *call == side),
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
            matches!(&e.body, Body::ModelResult { r#for: _, failure: None, call: c, outcome: Outcome::Interrupted { .. } } if *c == call)
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

struct Index;
impl Extension for Index {
    const NAME: &'static str = "index";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.text(ext::Text::generation("index", 0, "available: review"));
    }
}

#[test]
fn a_compaction_retry_republishes_the_index_and_keeps_the_summary() {
    let http = Flaky::new([
        Reply::Status(413, &[]),
        Reply::Events(text_reply()),
        Reply::Events(text_reply()),
    ]);
    let registry = Registry::default()
        .with(&Index)
        .unwrap()
        .with(&ext_compact::Compact::default())
        .unwrap();
    let session = fixture(registry, &http).start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(http.sent(), 3, "{log:#?}");
    assert_eq!(
        log.iter()
            .filter(|e| matches!(&e.body,
        Body::Context { text, .. } if text == "available: review"))
            .count(),
        2
    );
    let bodies = http.bodies();
    let resent = bodies[2].to_string();
    assert!(resent.contains("available: review"), "{resent}");
    assert!(resent.contains("summary of the conversation"), "{resent}");
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

struct Probe;

impl Tool for Probe {
    type In = ext::Args;
    type Out = String;
    const NAME: &'static str = "side-probe";
    const DESCRIPTION: &'static str = "ask a side model";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: ext::Concurrency = ext::Concurrency::Shared;

    async fn call(&self, args: &ext::Args, cx: &Cx) -> Result<String, String> {
        if args.args == "edit" {
            cx.edit(
                Seq::new(1).unwrap(),
                Seq::new(cx.call.get() - 1),
                Some("summary"),
            );
            return Ok("edited".into());
        }
        let mut side = ext::ModelTurn::new::<ext::ModelOnly>(
            ext::ThreadKey::new::<Probes>("probe", ext::OwnerScope::Call(cx.call)),
            ext::TurnKey("initial".into()),
            ext::ModelRole::Session,
            ext::SideContext::None,
            [llm::Message::user("Say hi.")].into_iter().collect(),
        );
        if args.args == "session" {
            side.thread.owner_scope = ext::OwnerScope::Session;
        }
        if args.args == "background" {
            let work = side.background().start(cx.clone()).work.unwrap();
            let completed = cx.service::<BackgroundCompletion>().map(|s| s.0.clone());
            cx.service::<dyn rt::Runtime>()
                .unwrap()
                .spawn(Box::pin(async move {
                    work.0.await;
                    if let Some(completed) = completed {
                        completed.set();
                    }
                }));
            return Ok("started".into());
        }
        if args.args == "duplicate" {
            let (done, other) = oneshot::channel();
            let owned = cx.clone();
            let again = side.clone();
            cx.service::<dyn rt::Runtime>()
                .unwrap()
                .spawn(Box::pin(async move {
                    done.send(owned.models().unwrap().call(&owned, again).await);
                }));
            let first = cx
                .models()
                .unwrap()
                .call(cx, side)
                .await
                .map_err(|e| e.to_string())?;
            let second = other.await.unwrap().map_err(|e| e.to_string())?;
            assert_eq!(first.message.text(), second.message.text());
            return Ok(first.message.text());
        }
        cx.models()
            .unwrap()
            .call(cx, side)
            .await
            .map(|r| r.message.text())
            .map_err(|e| e.to_string())
    }
}

struct Probes;

impl ext::Agent for Probes {
    fn step(&mut self, input: ext::Input, _: &dyn ext::LoopPoints) -> ext::Actions {
        let ext::Input::User { text } = input else {
            return ext::Actions::new();
        };
        (0..text.parse::<usize>().unwrap_or(1))
            .map(|_| ext::Action::Call {
                tool: "side-probe".into(),
                input: Json::encode(&ext::Args { args: text.clone() }).unwrap(),
                id: None,
            })
            .collect()
    }
}

impl Extension for Probes {
    const NAME: &'static str = "probes";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(Probe).agent("probes", || Box::new(Probes));
    }
}

fn probes(http: &Arc<Flaky>, registry: Registry) -> Fixture {
    Fixture::new(
        "probes",
        registry.with(&Probes).unwrap(),
        Models {
            http: Arc::clone(http) as Arc<dyn Http>,
            ..openai(&Script::new(&[]))
        },
    )
}

#[test]
fn repeating_a_turn_key_never_calls_the_provider_again() {
    let http = Flaky::new([Reply::Events(text_reply())]);
    let session = probes(&http, Registry::default()).start(&[]);
    session.submit("1").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    let prefix: Vec<_> = log
        .into_iter()
        .take_while(|e| !matches!(e.body, Body::ToolResult { .. }))
        .collect();
    assert!(
        prefix
            .iter()
            .any(|e| matches!(e.body, Body::ModelResult { .. }))
    );

    let replay = Flaky::new([]);
    let session = probes(&replay, Registry::default()).start(&prefix);
    let log = log_of(&session);
    session.halt();
    assert_eq!(replay.sent(), 0);
    assert_eq!(sides(&log).len(), 1);
    assert!(log.iter().any(|e| matches!(&e.body,
        Body::ToolResult { outcome: Outcome::Done { output }, .. }
            if output.raw().contains("crimson-harbor"))));
}

#[test]
fn ambiguous_recovery_never_re_spends() {
    let http = Flaky::new([Reply::Hang(String::new())]);
    let session = probes(&http, Registry::default()).start(&[]);
    session.submit("1").unwrap();
    http.wait_sent(1);
    let prefix = log_of(&session);
    let child = session
        .1
        .loaded()
        .into_iter()
        .find(|live| live.parent.is_some_and(|p| p.session == session.id))
        .unwrap();
    let child_header = Header {
        id: child.id,
        workspace: child.workspace.clone(),
        parent: child.parent,
    };
    let child_prefix = log_of(&child);
    session.halt();
    let replay = Flaky::new([]);
    let recovered = probes(&replay, Registry::default());
    recovered
        .0
        .store
        .import(&child_header, &child_prefix)
        .unwrap();
    let child = Live::start(&child_header, &recovered.0, Open::Load, Owner::Present).unwrap();
    settle(&child);
    assert!(log_of(&child).iter().any(|e| matches!(
        e.body,
        Body::ModelResult {
            outcome: Outcome::Interrupted { .. },
            ..
        }
    )));
    child.halt();
    let session = recovered.start(&prefix);
    let log = log_of(&session);
    session.halt();
    assert_eq!(replay.sent(), 0);
    let side = sides(&log)[0].0;
    assert!(log.iter().any(|e| matches!(&e.body,
        Body::ModelResult { call, outcome: Outcome::Interrupted { .. }, .. } if *call == side)));
    assert!(log.iter().any(|e| matches!(
        e.body,
        Body::ToolResult {
            outcome: Outcome::Failed { .. },
            ..
        }
    )));
}

#[test]
fn a_side_call_dies_with_its_session() {
    let http = Flaky::new([Reply::Hang(String::new())]);
    let session = probes(&http, Registry::default()).start(&[]);
    session.submit("background").unwrap();
    http.wait_sent(1);
    until(&session, "owner done", |log| {
        log.iter().any(|e| {
            matches!(
                e.body,
                Body::ToolResult {
                    outcome: Outcome::Done { .. },
                    ..
                }
            )
        })
    });
    let mut ledger = Ledger::default();
    for entry in log_of(&session) {
        ledger.apply(&entry);
    }
    assert_eq!(ledger.activity(), Activity::Idle);
    session.halt();
    let log = log_of(&session);
    assert!(log.iter().any(|e| matches!(
        e.body,
        Body::ModelResult {
            outcome: Outcome::Interrupted { .. },
            ..
        }
    )));
    assert_eq!(http.sent(), 1);
}

#[test]
fn retry_policy_applies_to_side_calls_without_reentering_compaction() {
    let http = Flaky::new([Reply::Status(429, &[]), Reply::Events(text_reply())]);
    let registry = Registry::default()
        .with(&ext_retry::Retry {
            base: Duration::ZERO,
            ..ext_retry::Retry::default()
        })
        .unwrap();
    let session = probes(&http, registry).start(&[]);
    session.submit("1").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(http.sent(), 2);
    let executor = session
        .1
        .loaded()
        .into_iter()
        .find(|live| live.parent.is_some_and(|p| p.session == session.id))
        .unwrap();
    let failures: Vec<_> = ext_logging::testing::pending().into_iter().filter(|fact| {
        matches!(fact, ext_logging::Fact::ProviderError { session: id, .. } if *id == executor.id)
    }).collect();
    assert!(
        matches!(
            failures.as_slice(),
            [ext_logging::Fact::ProviderError {
                kind: ext_logging::Provider::RateLimit,
                status: Some(429),
                retryable: true,
                ..
            }]
        ),
        "{failures:?}"
    );
    assert_eq!(sides(&log).len(), 1);
    let executed = log_of(&executor);
    let failure = executed
        .iter()
        .find_map(|e| match &e.body {
            Body::Decision {
                d:
                    Verdict::SideRetry {
                        attempt: 1,
                        failure,
                        ..
                    },
                ..
            } => Some(failure),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        llm::Error::from_json(failure.raw()).unwrap().kind,
        llm::Kind::RateLimit
    );
    assert!(log.iter().any(|e| matches!(
        e.body,
        Body::ToolResult {
            outcome: Outcome::Done { .. },
            ..
        }
    )));
}

#[test]
fn side_concurrency_is_bounded_and_rejection_is_a_typed_fact() {
    let http = Flaky::new((0..4).map(|_| Reply::Hang(String::new())));
    let session = probes(&http, Registry::default()).start(&[]);
    session.submit("5").unwrap();
    until(&session, "side limit", |log| {
        log.iter().any(|e| {
            matches!(
                e.body,
                Body::ModelResult {
                    failure: Some(_),
                    ..
                }
            )
        })
    });
    http.wait_sent(4);
    let log = log_of(&session);
    session.halt();
    assert_eq!(http.sent(), 4);
    let failure = log
        .iter()
        .find_map(|e| match &e.body {
            Body::ModelResult {
                failure: Some(failure),
                ..
            } => Some(failure),
            _ => None,
        })
        .unwrap();
    let failure = llm::Error::from_json(failure.raw()).unwrap();
    assert_eq!(failure.kind, llm::Kind::InvalidRequest);
    assert!(failure.message.contains("four side calls"));
}

#[test]
fn fixtures_answer_side_calls_through_the_durable_service() {
    let http = Flaky::new([]);
    let registry = Registry::default().with(&ext_fixtures::Fixtures).unwrap();
    let session = probes(&http, registry).start(&[]);
    session.submit("1").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(http.sent(), 0);
    let view = SessionView::from_log(SessionId::new(), "/w".into(), &log);
    assert_eq!(view.side("probe"), Some("fixture probe"));
}

#[test]
fn concurrent_consumers_share_one_side_intent_and_result() {
    let http = Flaky::new([Reply::Events(text_reply())]);
    let session = probes(&http, Registry::default()).start(&[]);
    session.submit("duplicate").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(http.sent(), 1);
    assert_eq!(sides(&log).len(), 1);
    assert!(log.iter().any(|e| matches!(
        e.body,
        Body::ToolResult {
            outcome: Outcome::Done { .. },
            ..
        }
    )));
}

#[test]
fn a_fast_role_uses_config_and_logs_its_prompt_separately_from_context() {
    let http = Flaky::new([Reply::Events(text_reply()), Reply::Events(text_reply())]);
    let registry = Registry::default().with(&ext_title::Title).unwrap();
    let harness = fixture(registry, &http);
    let path = harness.0.models.settings.home.path();
    let mut config = crate::config::File::read(path).unwrap();
    config.fast_model = Some("openai:side-fast".into());
    config.write(path).unwrap();
    let session = harness.start(&[]);
    session.submit("my distinct task").unwrap();
    until(&session, "title result", |log| {
        sides(log).first().is_some_and(|&(side, ..)| {
            log.iter()
                .any(|e| matches!(&e.body, Body::ModelResult { call, .. } if *call == side))
        })
    });
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(
        http.bodies()
            .iter()
            .filter(|b| b["model"] == "side-fast")
            .count(),
        1
    );
    let intent = log
        .iter()
        .find_map(|e| match &e.body {
            Body::ModelCall {
                purpose: Some(p),
                request: Some(request),
                model,
                ..
            } if p == "title" => {
                assert_eq!(model, "openai:side-fast");
                Some(protocol::publication::ThreadImage::from_json(request).unwrap())
            }
            _ => None,
        })
        .unwrap();
    let (turn, prefix) = ext::ModelTurn::from_intent(intent.turn.raw()).unwrap();
    assert!(matches!(turn.role, ext::ModelRole::Fast));
    assert!(
        !Json::encode(&turn.messages)
            .unwrap()
            .raw()
            .contains("my distinct task")
    );
    assert!(prefix.is_some());
    assert!(!intent.turn.raw().contains("my distinct task"));
    let view = SessionView::from_log(SessionId::new(), "/w".into(), &log);
    assert!(view.items.iter().any(|i| matches!(i,
        protocol::view::Item::Reply { purpose: Some(p), usage: Some(_), .. } if p == "title")));
    std::fs::remove_file(path).unwrap();
}

struct FailResultCommit(Arc<crate::store::Faulty>, Arc<AtomicUsize>);

impl ext::SideProvider for FailResultCommit {
    fn reply<'a>(
        &'a self,
        side: &'a ext::ModelTurn,
        messages: &'a llm::Messages,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        Box::pin(async move {
            let reply = ext::SideProvider::reply(&ext_fixtures::Fixtures, side, messages).await;
            if self.1.fetch_add(1, Ordering::SeqCst) == 0 {
                self.0.fail_next_sync(crate::store::SyncFailure::Discard);
            }
            reply
        })
    }
}

impl Extension for FailResultCommit {
    const NAME: &'static str = "fail-side-result";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn ext::SideProvider>(Arc::new(Self(
            Arc::clone(&self.0),
            Arc::clone(&self.1),
        )));
    }
}

#[test]
fn a_failed_result_commit_cannot_release_a_successful_side_reply() {
    let faulty = Arc::new(crate::store::Faulty::new(Arc::new(Memory::default())));
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = Registry::default()
        .with(&Probes)
        .unwrap()
        .with(&FailResultCommit(Arc::clone(&faulty), Arc::clone(&calls)))
        .unwrap();
    let harness = Fixture::on(faulty, "probes", registry, openai(&Script::new(&[])));
    let session = harness.start(&[]);
    session.submit("1").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(sides(&log).len(), 1);
    assert!(log.iter().any(|e| matches!(
        e.body,
        Body::ModelResult {
            outcome: Outcome::Interrupted { .. },
            ..
        }
    )));
    assert!(!log.iter().any(|e| matches!(
        e.body,
        Body::ToolResult {
            outcome: Outcome::Done { .. },
            ..
        }
    )));
}

struct BackgroundCompletion(Flag);

impl Extension for BackgroundCompletion {
    const NAME: &'static str = "background-completion";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide(Arc::new(Self(self.0.clone())));
    }
}

#[test]
fn shutdown_never_admits_a_background_side_call_waiting_for_its_flush() {
    let faulty = Arc::new(crate::store::Faulty::new(Arc::new(Memory::default())));
    let http = Flaky::new([Reply::Events(text_reply())]);
    let completed = Flag::default();
    let harness = Fixture::on(
        Arc::clone(&faulty) as _,
        "probes",
        Registry::default()
            .with(&Probes)
            .unwrap()
            .with(&BackgroundCompletion(completed.clone()))
            .unwrap(),
        Models {
            http: Arc::clone(&http) as Arc<dyn Http>,
            ..openai(&Script::new(&[]))
        },
    );
    let session = harness.create();
    settle(&session);
    let flush = faulty.hold_next_flush();
    let (reply, ack) = oneshot::channel();
    session.send(Msg::Request {
        request: Request::Input {
            text: "background".into(),
            mode: Mode::Queue,
        },
        origin: None,
        reply,
    });
    flush.started();
    session.wait(
        "background intent ordered after owner finished",
        PATIENCE,
        || !session.has_work() && !sides(&faulty.pending(session.id)).is_empty(),
    );
    assert_eq!(http.sent(), 0);
    session.stop();
    flush.release();
    rt::block_on(session.ended.wait());
    let _ = rt::block_on(ack);
    assert!(rt::block_on(first(completed.wait(), harness.0.rt.sleep(PATIENCE))).is_some());
    assert_eq!(
        http.sent(),
        0,
        "shutdown must not spend on an unadmitted side call"
    );
}
struct DelayedSide(Flag);

impl ext::SideProvider for DelayedSide {
    fn reply<'a>(
        &'a self,
        side: &'a ext::ModelTurn,
        messages: &'a llm::Messages,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        Box::pin(async move {
            self.0.wait().await;
            ext::SideProvider::reply(&ext_fixtures::Fixtures, side, messages).await
        })
    }
}

impl Extension for DelayedSide {
    const NAME: &'static str = "delayed-side";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn ext::SideProvider>(Arc::new(Self(self.0.clone())));
    }
}

#[test]
fn a_background_result_stays_out_of_context_after_its_intent_is_compacted() {
    let release = Flag::default();
    let http = Flaky::new([]);
    let registry = Registry::default()
        .with(&DelayedSide(release.clone()))
        .unwrap()
        .with(&ext_fork::Fork)
        .unwrap();
    let fixture = probes(&http, registry);
    let session = fixture.start(&[]);
    session.submit("background").unwrap();
    until(&session, "background intent", |log| !sides(log).is_empty());
    settle(&session);
    session.submit("edit").unwrap();
    settle(&session);
    release.set();
    until(&session, "background result", |log| {
        let side = sides(log)[0].0;
        log.iter()
            .any(|e| matches!(e.body, Body::ModelResult { call, .. } if call == side))
    });
    let log = log_of(&session);
    session.halt();
    let messages = transcript(&log, log.len() as u64 + 1, Arc::new(Registry::default()));
    assert_eq!(messages, [llm::Message::user("summary")]);
    let header = fixture
        .0
        .store
        .headers()
        .unwrap()
        .into_iter()
        .find(|header| header.id == session.id)
        .unwrap();
    let fork = copy_history(
        &header,
        log.last().unwrap().seq,
        SessionId::new(),
        &fixture.0,
    )
    .unwrap();
    let copied = fixture.0.store.entries(fork.id).unwrap();
    let messages = transcript(
        &copied,
        copied.len() as u64 + 1,
        Arc::new(Registry::default()),
    );
    assert_eq!(messages, [llm::Message::user("summary")]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn compacted_forks_keep_loaded_skills_in_context_and_background_results_out() {
    let home = std::env::temp_dir().join(format!("a-side-skills-{}", SessionId::new()));
    let workspace = home.join("ws");
    let directory = workspace.join(".a/skills/review");
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("SKILL.md");
    std::fs::write(
        &file,
        "---\nname: review\ndescription: Review a change\n---\nprivate review instructions",
    )
    .unwrap();
    let release = Flag::default();
    let base = Registry::default()
        .with(&Probes)
        .unwrap()
        .with(&DelayedSide(release.clone()))
        .unwrap()
        .with(&ext_fork::Fork)
        .unwrap();
    let loader = Arc::new(ext::host::Coordinator::new(
        base.clone(),
        vec![Arc::new(ext_skills::Loader::new(
            &home,
            ext_skills::Skills::default(),
        ))],
    ));
    let fixture = Fixture::new(
        "probes",
        base.with(&loader).unwrap(),
        openai(&Script::new(&[])),
    );
    let header = Header {
        workspace,
        ..Fixture::header()
    };
    let slot = fixture.2.new_slot(header.clone()).unwrap();
    let session = Live::start(&header, &fixture.0, Open::Create, Owner::None).unwrap();
    fixture.2.started(&slot, Ok(Arc::clone(&session))).unwrap();
    let load = |session: &Arc<Live>, expected: &str| {
        session.submit("/review").unwrap();
        settle(session);
        let log = log_of(session);
        let output = log
            .iter()
            .rev()
            .find_map(|e| match &e.body {
                Body::ToolResult {
                    outcome: Outcome::Done { output },
                    ..
                } => Some(output),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            output.parse::<Value>().unwrap(),
            serde_json::json!({"Ok": expected})
        );
    };
    load(&session, "loaded");
    session.submit("background").unwrap();
    until(&session, "background intent", |log| !sides(log).is_empty());
    settle(&session);
    session.submit("edit").unwrap();
    settle(&session);
    assert_eq!(
        transcript(&log_of(&session), u64::MAX, Arc::new(Registry::default())),
        [llm::Message::user("summary")]
    );
    load(&session, "loaded");
    release.set();
    until(&session, "background result", |log| {
        let side = sides(log)[0].0;
        log.iter()
            .any(|e| matches!(e.body, Body::ModelResult { call, .. } if call == side))
    });
    let original = log_of(&session);
    session.halt();
    // Restoration must use the installation image, not rediscover this body.
    std::fs::remove_file(file).unwrap();
    let fork = copy_history(
        &header,
        original.last().unwrap().seq,
        SessionId::new(),
        &fixture.0,
    )
    .unwrap();
    let branch = Live::start(&fork, &fixture.0, Open::Load, Owner::None).unwrap();
    settle(&branch);
    load(&branch, "already_loaded");
    let copied = log_of(&branch);
    branch.halt();
    assert_eq!(&copied[..original.len()], &original);
    let messages = transcript(&copied, u64::MAX, Arc::new(Registry::default()));
    assert_eq!(messages.len(), 4, "{messages:#?}");
    assert!(format!("{:?}", messages[0]).contains("private review instructions"));
    assert_eq!(messages[1], llm::Message::user("summary"));
    assert_eq!(messages[2], llm::Message::user("/review"));
    assert_eq!(messages[3], llm::Message::user("/review"));
    let view = SessionView::from_log(fork.id, fork.workspace, &copied);
    assert_eq!(view.side("probe"), Some("fixture probe"));
    drop(branch);
    drop(session);
    drop(fixture);
    std::fs::remove_dir_all(home).unwrap();
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
        assert!(
            super::super::side::sided(session, Some(Err(error)))
                .1
                .is_err()
        );
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
    assert!(super::super::side::sided(session, None).1.is_err());
    assert!(
        ext_logging::testing::pending()
            .iter()
            .any(|fact| matches!(fact,
        ext_logging::Fact::ProviderError { session: id, kind: ext_logging::Provider::Transport, .. }
        if *id == session))
    );
}

#[test]
fn cancellation_is_not_reported_as_a_side_provider_failure() {
    let session = SessionId::new();
    assert!(
        super::super::side::sided(
            session,
            Some(Ok((Outcome::Interrupted { output: None }, None)))
        )
        .1
        .is_err()
    );
    assert!(!ext_logging::testing::pending().iter().any(|fact| {
        matches!(fact, ext_logging::Fact::ProviderError { session: id, .. } if *id == session)
    }));
}

struct HeldDescription {
    started: Flag,
    release: Flag,
    tool_release: Flag,
}

impl ext::SideProvider for HeldDescription {
    fn reply<'a>(
        &'a self,
        side: &'a ext::ModelTurn,
        messages: &'a llm::Messages,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        Box::pin(async move {
            assert_eq!(side.thread.purpose, "approval-description");
            assert!(matches!(side.role, ext::ModelRole::Fast));
            assert!(matches!(side.context, ext::SideContext::None));
            assert!(side.is_while_waiting());
            assert!(
                side.messages.iter().any(|message| match message {
                    llm::Message::User(user) => user.content.iter().any(|part| matches!(part, llm::UserPart::Text(text) if text.text.contains(r#""seconds":7"#))),
                    _ => false,
                }),
                "the description must use the input the gate left behind"
            );
            self.started.set();
            self.release.wait().await;
            ext::SideProvider::reply(&ext_fixtures::Fixtures, side, messages).await
        })
    }
}

impl Extension for HeldDescription {
    const NAME: &'static str = "held-description";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<point::PreTool>(|_, call| {
            if call.tool == "sleep" {
                Rewrite::Replace(Json::encode(&json!({"seconds":7})).unwrap())
            } else {
                Rewrite::Pass
            }
        });
        r.tool(ApprovalWork(self.tool_release.clone()))
            .agent(ext_fixtures::ECHO, || Box::new(ext_fixtures::Echo))
            .provide::<dyn ext::SideProvider>(Arc::new(Self {
                started: self.started.clone(),
                release: self.release.clone(),
                tool_release: self.tool_release.clone(),
            }));
    }
}

#[test]
fn answering_an_approval_cancels_its_description_without_cancelling_the_tool() {
    for approved in [true, false] {
        let started = Flag::default();
        let release = Flag::default();
        let tool_release = Flag::default();
        let registry = Registry::default()
            .with(&Policy {
                describe: true,
                ..Policy::ask(&["sleep"])
            })
            .unwrap()
            .with(&HeldDescription {
                started: started.clone(),
                release,
                tool_release: tool_release.clone(),
            })
            .unwrap();
        let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
        let session = fixture.start(&[]);
        session.submit("sleep 60").unwrap();
        let asked = until(&session, "approval before description", |log| {
            !inbox(log).is_empty()
        });
        let view = SessionView::from_log(session.id, session.workspace.clone(), &asked);
        let tool = view
            .items
            .iter()
            .find(|i| matches!(i, protocol::view::Item::Tool { .. }))
            .unwrap();
        let shown = view.present(tool);
        assert!(matches!(shown.buttons, protocol::view::Buttons::Approve(_)));
        assert!(
            client::approval_text(&shown)
                .ends_with("Why: the ask mode requires approval for this tool")
        );
        assert!(rt::block_on(first(started.wait(), fixture.0.rt.sleep(PATIENCE))).is_some());
        let call = inbox(&asked)[0];
        session.answer(call, approved).unwrap();
        let cancelled = until(&session, "description cancelled", |log| {
            let Some((side, _, _)) = sides(log)
                .into_iter()
                .find(|(_, _, purpose)| *purpose == "approval-description")
            else {
                return false;
            };
            log.iter().any(|entry| matches!(&entry.body, Body::ModelResult { call, outcome: Outcome::Interrupted { .. }, .. } if *call == side))
        });
        if approved {
            assert!(!cancelled.iter().any(
                |entry| matches!(&entry.body, Body::ToolResult { call: c, .. } if c.get() == call)
            ));
            tool_release.set();
        }
        let done = finished(&session, call);
        assert_eq!(done.iter().any(|entry| matches!(&entry.body, Body::ToolResult { call: c, outcome: Outcome::Done { .. }, .. } if c.get() == call)), approved);
        session.halt();
    }
}

struct ApprovalWork(Flag);
impl Tool for ApprovalWork {
    type In = ext_fixtures::Seconds;
    type Out = String;
    const NAME: &'static str = "sleep";
    const DESCRIPTION: &'static str = "Wait for the test to release work.";
    const REPLAY: Replay = Replay::Safe;
    async fn call(&self, _: &Self::In, _: &Cx) -> Result<String, String> {
        self.0.wait().await;
        Ok("done".into())
    }
}

#[test]
fn title_regeneration_appends_to_the_same_thread() {
    let http = Flaky::new((0..4).map(|_| Reply::Events(text_reply())));
    let fixture = fixture(Registry::default().with(&ext_title::Title).unwrap(), &http);
    let session = fixture.start(&[]);
    session.submit("my distinct task").unwrap();
    until(&session, "initial title", |log| {
        SessionView::from_log(session.id, "/w".into(), log)
            .side("title")
            .is_some()
    });
    settle(&session);
    let initial = fixture
        .2
        .loaded()
        .into_iter()
        .find(|live| live.parent.is_some_and(|p| p.session == session.id))
        .unwrap();
    fixture.2.slot(initial.id).unwrap().unload();
    rt::block_on(initial.ended.wait());
    session.submit("/title").unwrap();
    settle(&session);
    session.submit("/title call it harbor work").unwrap();
    settle(&session);
    let log = log_of(&session);
    let images: Vec<_> = log
        .iter()
        .filter_map(|e| match &e.body {
            Body::ModelCall {
                r#for: Some(_),
                request: Some(request),
                ..
            } => Some(protocol::publication::ThreadImage::from_json(request).unwrap()),
            _ => None,
        })
        .collect();
    assert_eq!(images.len(), 3);
    assert!(images.iter().all(|image| {
        ext::SideIdentity::from_intent(&image.to_json().unwrap())
            .unwrap()
            .thread
            == ext::SideIdentity::from_intent(&images[0].to_json().unwrap())
                .unwrap()
                .thread
    }));
    let child = fixture
        .2
        .slot(
            ext::SideIdentity::from_intent(&images[0].to_json().unwrap())
                .unwrap()
                .thread
                .conversation(session.id),
        )
        .unwrap()
        .live()
        .unwrap();
    let log = log_of(&child);
    assert_eq!(
        log.iter()
            .filter(|e| matches!(e.body, Body::Enqueue { .. }))
            .count(),
        3
    );
    assert_eq!(
        log.iter()
            .filter(|e| matches!(
                e.body,
                Body::ModelResult {
                    outcome: Outcome::Done { .. },
                    ..
                }
            ))
            .count(),
        3
    );
    assert_eq!(http.sent(), 4);
    session.halt();
}

struct TitleHistory(Arc<Mutex<Vec<llm::Messages>>>);
impl ext::SideProvider for TitleHistory {
    fn reply<'a>(
        &'a self,
        turn: &'a ext::ModelTurn,
        messages: &'a llm::Messages,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        lock(&self.0).push(messages.clone());
        ext::SideProvider::reply(&ext_fixtures::Fixtures, turn, messages)
    }
}
impl Extension for TitleHistory {
    const NAME: &'static str = "title-history";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn ext::SideProvider>(Arc::new(Self(Arc::clone(&self.0))));
    }
}

#[test]
fn new_title_turn_sees_previous_exchanges() {
    let http = Flaky::new([Reply::Events(text_reply())]);
    let messages = Arc::new(Mutex::new(Vec::new()));
    let registry = Registry::default()
        .with(&ext_title::Title)
        .unwrap()
        .with(&TitleHistory(Arc::clone(&messages)))
        .unwrap();
    let session = fixture(registry, &http).start(&[]);
    session.submit("my distinct task").unwrap();
    until(&session, "initial title", |log| {
        SessionView::from_log(session.id, "/w".into(), log)
            .side("title")
            .is_some()
    });
    settle(&session);
    session.submit("/title use a shorter name").unwrap();
    settle(&session);
    let messages = lock(&messages);
    assert_eq!(messages.len(), 2);
    let regenerated = format!("{:?}", messages[1]);
    assert!(
        messages[1]
            .iter()
            .any(|m| matches!(m, llm::Message::Assistant(a) if a.text() == "fixture title")),
        "{regenerated}"
    );
    assert!(regenerated.contains("my distinct task"), "{regenerated}");
    assert!(regenerated.contains("use a shorter name"), "{regenerated}");
    session.halt();
}

#[test]
fn model_only_threads_offer_no_tools() {
    let http = Flaky::new([Reply::Events(text_reply())]);
    let session = probes(&http, Registry::default().with(&Harbor).unwrap()).start(&[]);
    session.submit("1").unwrap();
    settle(&session);
    let bodies = http.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0]
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "{}",
        bodies[0]
    );
    assert!(
        !log_of(&session)
            .iter()
            .any(|e| matches!(&e.body, Body::ToolCall { tool, .. } if tool == HarborLabel::NAME))
    );
    session.halt();
}

#[test]
fn successful_owner_completion_keeps_background_work_but_interrupt_cancels_it() {
    let http = Flaky::new([Reply::Hang(String::new())]);
    let session = probes(&http, Registry::default()).start(&[]);
    session.submit("background").unwrap();
    http.wait_sent(1);
    settle(&session);
    let log = log_of(&session);
    assert!(log.iter().any(|e| matches!(
        e.body,
        Body::ToolResult {
            outcome: Outcome::Done { .. },
            ..
        }
    )));
    assert!(
        !log.iter()
            .any(|e| matches!(e.body, Body::ModelResult { .. }))
    );
    session.interrupt_now();
    until(&session, "side cancelled", |log| {
        log.iter().any(|e| {
            matches!(
                e.body,
                Body::ModelResult {
                    outcome: Outcome::Interrupted { .. },
                    ..
                }
            )
        })
    });
    assert_eq!(http.sent(), 1);
    session.halt();
}

#[test]
fn session_turn_keys_are_reused_across_distinct_owners() {
    let http = Flaky::new([Reply::Events(text_reply())]);
    let session = probes(&http, Registry::default()).start(&[]);
    for _ in 0..2 {
        session.submit("session").unwrap();
        settle(&session);
    }
    let log = log_of(&session);
    assert_eq!(http.sent(), 1);
    assert_eq!(sides(&log).len(), 1);
    assert_eq!(
        log.iter()
            .filter(|e| matches!(
                e.body,
                Body::ToolResult {
                    outcome: Outcome::Done { .. },
                    ..
                }
            ))
            .count(),
        2
    );
    session.halt();
}

#[test]
fn cancelled_queued_turns_never_reappear_after_restart() {
    let http = Flaky::new([Reply::Hang(String::new())]);
    let session = probes(&http, Registry::default()).start(&[]);
    session.submit("background").unwrap();
    http.wait_sent(1);
    let child = session
        .1
        .loaded()
        .into_iter()
        .find(|live| live.parent.is_some_and(|p| p.session == session.id))
        .unwrap();
    rt::block_on(child.enqueue(protocol::Envelope {
        id: protocol::EnvelopeId {
            sender: session.id,
            call: CallId::new(999).unwrap(),
            purpose: "queued".into(),
        },
        destination: child.id,
        payload: "queued work".into(),
    }))
    .unwrap();
    session.interrupt_now();
    let prefix = until(&child, "queued work discarded", |log| {
        log.iter()
            .filter(|e| matches!(e.body, Body::Take { .. }))
            .count()
            == 2
            && matches!(log.last().map(|e| &e.body), Some(Body::Interrupt { .. }))
    });
    let header = Header {
        id: child.id,
        workspace: child.workspace.clone(),
        parent: child.parent,
    };
    session.halt();
    let replay = Flaky::new([]);
    let fixture = probes(&replay, Registry::default());
    fixture.0.store.import(&header, &prefix).unwrap();
    let recovered = Live::start(&header, &fixture.0, Open::Load, Owner::Present).unwrap();
    settle(&recovered);
    assert_eq!(replay.sent(), 0);
    assert!(
        !log_of(&recovered)
            .iter()
            .any(|e| e.seq.get() > prefix.last().unwrap().seq.get()
                && matches!(e.body, Body::ModelCall { .. }))
    );
    recovered.halt();
}

#[test]
fn forked_title_regeneration_never_writes_the_sources_thread() {
    let http = Flaky::new((0..4).map(|_| Reply::Events(text_reply())));
    let fixture = fixture(
        Registry::default()
            .with(&ext_title::Title)
            .unwrap()
            .with(&ext_fork::Fork)
            .unwrap(),
        &http,
    );
    let session = fixture.start(&[]);
    session.submit("source task").unwrap();
    until(&session, "initial title", |log| {
        SessionView::from_log(session.id, "/w".into(), log)
            .side("title")
            .is_some()
    });
    settle(&session);
    let child = fixture
        .2
        .loaded()
        .into_iter()
        .find(|live| live.parent.is_some_and(|p| p.session == session.id))
        .unwrap();
    let before = log_of(&child);
    let header = fixture
        .0
        .store
        .headers()
        .unwrap()
        .into_iter()
        .find(|header| header.id == session.id)
        .unwrap();
    let source_log = log_of(&session);
    let fork = copy_history(
        &header,
        source_log.last().unwrap().seq,
        SessionId::new(),
        &fixture.0,
    )
    .unwrap();
    let slot = fixture.2.new_slot(fork.clone()).unwrap();
    let branch = Live::start(&fork, &fixture.0, Open::Load, Owner::None).unwrap();
    let branch = fixture.2.started(&slot, Ok(branch)).unwrap();
    branch.submit("/title fork title").unwrap();
    settle(&branch);
    let after = log_of(&child);
    branch.halt();
    session.halt();
    assert_eq!(
        after.len(),
        before.len(),
        "the fork appended work to the source-owned thread"
    );
}
#[test]
fn recovered_side_inbox_never_spends_an_abandoned_turn() {
    let http = Flaky::new([Reply::Hang(String::new())]);
    let session = probes(&http, Registry::default()).start(&[]);
    session.submit("1").unwrap();
    http.wait_sent(1);
    let parent_prefix = log_of(&session);
    let child = session
        .1
        .loaded()
        .into_iter()
        .find(|live| live.parent.is_some_and(|p| p.session == session.id))
        .unwrap();
    let header = Header {
        id: child.id,
        workspace: child.workspace.clone(),
        parent: child.parent,
    };
    let child_prefix: Vec<_> = log_of(&child)
        .into_iter()
        .take_while(|entry| !matches!(entry.body, Body::ModelCall { .. }))
        .collect();
    assert!(
        child_prefix
            .iter()
            .any(|entry| matches!(entry.body, Body::Enqueue { .. }))
    );
    session.halt();
    let replay = Flaky::new([Reply::Events(text_reply())]);
    let recovered = probes(&replay, Registry::default());
    recovered.0.store.import(&header, &child_prefix).unwrap();
    let parent_header = Header {
        id: session.id,
        workspace: session.workspace.clone(),
        parent: None,
    };
    recovered
        .0
        .store
        .import(&parent_header, &parent_prefix)
        .unwrap();
    let slot = recovered.2.new_slot(parent_header.clone()).unwrap();
    let owner = Live::start(&parent_header, &recovered.0, Open::Load, Owner::None).unwrap();
    let owner = recovered.2.started(&slot, Ok(owner)).unwrap();
    settle(&owner);
    let owner_state = recovered.0.owner(&header).unwrap();
    let child = Live::start(&header, &recovered.0, Open::Load, owner_state).unwrap();
    settle(&child);
    let spent = replay.sent();
    child.halt();
    owner.halt();
    assert_eq!(
        spent, 0,
        "a turn already interrupted by parent recovery spent on child reload"
    );
}
struct TitleLengths(Arc<Mutex<Vec<(usize, usize)>>>);
impl ext::SideProvider for TitleLengths {
    fn reply<'a>(
        &'a self,
        turn: &'a ext::ModelTurn,
        messages: &'a llm::Messages,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        lock(&self.0).push((messages.len(), Json::encode(messages).unwrap().raw().len()));
        ext::SideProvider::reply(&ext_fixtures::Fixtures, turn, messages)
    }
}
impl Extension for TitleLengths {
    const NAME: &'static str = "title-lengths";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn ext::SideProvider>(Arc::new(Self(Arc::clone(&self.0))));
    }
}

#[test]
fn title_regeneration_keeps_owner_context_out_of_thread_history() {
    let http = Flaky::new([Reply::Events(text_reply())]);
    let lengths = Arc::new(Mutex::new(Vec::new()));
    let registry = Registry::default()
        .with(&ext_title::Title)
        .unwrap()
        .with(&TitleLengths(Arc::clone(&lengths)))
        .unwrap();
    let fixture = fixture(registry, &http);
    let session = fixture.start(&[]);
    session.submit("source task").unwrap();
    until(&session, "initial title", |log| {
        SessionView::from_log(session.id, "/w".into(), log)
            .side("title")
            .is_some()
    });
    settle(&session);
    for _ in 0..1000 {
        session.submit("/title").unwrap();
        settle(&session);
    }
    let child = fixture
        .2
        .loaded()
        .into_iter()
        .find(|live| live.parent.is_some_and(|p| p.session == session.id))
        .unwrap();
    let entries = log_of(&child);
    let messages = Transcript::of(&entries, Arc::new(Registry::default()), "").into_messages();
    let copies = messages
        .iter()
        .filter(|message| {
            matches!(message,
        llm::Message::User(user) if user.content.iter().any(|part| matches!(part,
            llm::UserPart::Text(text) if text.text == "source task")))
        })
        .count();
    let lengths = lock(&lengths);
    session.halt();
    assert_eq!(lengths.len(), 1001);
    assert_eq!(copies, 0);
    assert_eq!(messages.len(), 2002);
    assert!(
        lengths
            .iter()
            .all(|&(messages, bytes)| messages <= 8 && bytes <= 8192)
    );
    assert!(lengths[10..].iter().all(|length| *length == lengths[10]));
    assert!(Json::encode(&entries).unwrap().raw().len() < 2_000_000);
}

#[test]
fn a_live_sender_is_not_an_abandoned_inbox_owner() {
    let http = Flaky::new([Reply::Hang(String::new())]);
    let fixture = probes(&http, Registry::default());
    let session = fixture.start(&[]);
    session.submit("1").unwrap();
    http.wait_sent(1);
    let child = fixture
        .2
        .loaded()
        .into_iter()
        .find(|live| live.parent.is_some_and(|p| p.session == session.id))
        .unwrap();
    let prefix: Vec<_> = log_of(&child)
        .into_iter()
        .take_while(|entry| !matches!(entry.body, Body::ModelCall { .. }))
        .collect();
    let mut ledger = Ledger::default();
    for entry in &prefix {
        ledger.apply(entry);
    }
    let header = Header {
        id: child.id,
        workspace: child.workspace.clone(),
        parent: child.parent,
    };
    let abandoned = fixture.0.abandoned(&header, &ledger).unwrap();
    assert!(
        !log_of(&session)
            .iter()
            .any(|entry| matches!(entry.body, Body::Interrupt { .. }))
    );
    session.halt();
    assert!(
        abandoned.is_empty(),
        "a live sender was treated as interrupted: {abandoned:?}"
    );
}

struct OwnerOnlyText;
impl Extension for OwnerOnlyText {
    const NAME: &'static str = "owner-only-text";
    const VERSION: u32 = 1;
    const INHERIT: bool = false;
    fn contribute(&self, r: &mut Registrar) {
        r.text(ext::Text::system(
            "owner-context",
            0,
            "owner transcript instruction",
        ));
    }
}

#[test]
fn before_owner_uses_the_owners_transcript_registry() {
    let http = Flaky::new([Reply::Events(text_reply())]);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let registry = Registry::default()
        .with(&ext_title::Title)
        .unwrap()
        .with(&OwnerOnlyText)
        .unwrap()
        .with(&TitleHistory(Arc::clone(&captured)))
        .unwrap();
    let fixture = fixture(registry, &http);
    let session = fixture.start(&[]);
    session.submit("source task").unwrap();
    until(&session, "initial title", |log| {
        SessionView::from_log(session.id, "/w".into(), log)
            .side("title")
            .is_some()
    });
    settle(&session);
    let actual = format!("{:?}", lock(&captured));
    session.halt();
    assert!(
        actual.contains("owner transcript instruction"),
        "the owner's system context was lost: {actual}"
    );
}

#[test]
fn forks_of_forks_keep_title_threads_separate() {
    let http = Flaky::new((0..6).map(|_| Reply::Events(text_reply())));
    let fixture = fixture(
        Registry::default()
            .with(&ext_title::Title)
            .unwrap()
            .with(&ext_fork::Fork)
            .unwrap(),
        &http,
    );
    let session = fixture.start(&[]);
    session.submit("source task").unwrap();
    until(&session, "initial title", |log| {
        SessionView::from_log(session.id, "/w".into(), log)
            .side("title")
            .is_some()
    });
    settle(&session);
    let fork = |source: &Live| {
        let header = fixture
            .0
            .store
            .headers()
            .unwrap()
            .into_iter()
            .find(|h| h.id == source.id)
            .unwrap();
        let log = log_of(source);
        let fork = copy_history(
            &header,
            log.last().unwrap().seq,
            SessionId::new(),
            &fixture.0,
        )
        .unwrap();
        let slot = fixture.2.new_slot(fork.clone()).unwrap();
        let live = Live::start(&fork, &fixture.0, Open::Load, Owner::None).unwrap();
        fixture.2.started(&slot, Ok(live)).unwrap()
    };
    let branch = fork(&session);
    branch.submit("/title branch").unwrap();
    settle(&branch);
    let source_children: Vec<_> = fixture
        .2
        .loaded()
        .into_iter()
        .filter(|l| l.parent.is_some())
        .map(|l| (l.id, log_of(&l)))
        .collect();
    let grandchild = fork(&branch);
    grandchild.submit("/title grandchild").unwrap();
    settle(&grandchild);
    for (id, before) in source_children {
        assert_eq!(log_of(&fixture.2.slot(id).unwrap().live().unwrap()), before);
    }
    assert!(
        fixture
            .2
            .loaded()
            .into_iter()
            .any(|l| l.parent.is_some_and(|p| p.session == grandchild.id))
    );
    grandchild.halt();
    branch.halt();
    session.halt();
}

#[test]
fn an_in_flight_title_cannot_be_copied_into_a_fork() {
    let http = Flaky::new([Reply::Events(text_reply()), Reply::Hang(String::new())]);
    let fixture = fixture(
        Registry::default()
            .with(&ext_title::Title)
            .unwrap()
            .with(&ext_fork::Fork)
            .unwrap(),
        &http,
    );
    let session = fixture.start(&[]);
    session.submit("source task").unwrap();
    http.wait_sent(2);
    settle(&session);
    let header = fixture
        .0
        .store
        .headers()
        .unwrap()
        .into_iter()
        .find(|h| h.id == session.id)
        .unwrap();
    let log = log_of(&session);
    let result = copy_history(
        &header,
        log.last().unwrap().seq,
        SessionId::new(),
        &fixture.0,
    );
    session.halt();
    assert!(matches!(result, Err(protocol::ForkError::Unfinished)));
}

#[test]
fn owner_sequence_context_survives_context_edits() {
    let http = Flaky::new([]);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let registry = Registry::default()
        .with(&ext_title::Title)
        .unwrap()
        .with(&TitleHistory(Arc::clone(&captured)))
        .unwrap();
    let fixture = probes(&http, registry);
    let session = fixture.start(&[]);
    session.submit("1").unwrap();
    settle(&session);
    session.submit("edit").unwrap();
    settle(&session);
    session.submit("/title after edit").unwrap();
    settle(&session);
    let actual = format!("{:?}", lock(&captured).last().unwrap());
    session.halt();
    assert!(actual.contains("summary"), "{actual}");
    assert!(actual.contains("after edit"), "{actual}");
}

#[test]
fn a_plain_model_only_input_is_never_dropped_by_the_context_bound() {
    let http = Flaky::new([Reply::Events(text_reply())]);
    let captured = Arc::new(Mutex::new(Vec::new()));
    let registry = Registry::default()
        .with(&ext_title::Title)
        .unwrap()
        .with(&TitleHistory(Arc::clone(&captured)))
        .unwrap();
    let fixture = fixture(registry, &http);
    let session = fixture.start(&[]);
    session.submit("source task").unwrap();
    until(&session, "initial title", |log| {
        SessionView::from_log(session.id, "/w".into(), log)
            .side("title")
            .is_some()
    });
    settle(&session);
    let child = fixture
        .2
        .loaded()
        .into_iter()
        .find(|live| live.parent.is_some_and(|p| p.session == session.id))
        .unwrap();
    let before = lock(&captured).len();
    child.submit(&"current instruction ".repeat(1000)).unwrap();
    settle(&child);
    let after = lock(&captured).len();
    let failure = log_of(&child)
        .into_iter()
        .rev()
        .find_map(|entry| match entry.body {
            Body::ModelResult {
                failure: Some(failure),
                ..
            } => Some(failure),
            _ => None,
        })
        .expect("mandatory input failure");
    assert_eq!(
        llm::Error::from_json(failure.raw()).unwrap().kind,
        llm::Kind::InvalidRequest
    );
    session.halt();
    assert_eq!(
        after, before,
        "an oversized mandatory raw input should fail before calling the provider"
    );
}
