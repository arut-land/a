use ext::host::Registry;
use ext::point::Failed;
use ext::testing::Rig;
use ext::{Asked, Tier};

use super::*;

/// Who answers a failure of `kind` of the rig's next call, and how.
fn answer(rig: &Rig, kind: llm::Kind) -> Option<(String, bool)> {
    let cx = rig.cx();
    let error = llm::Error::new(kind, "x");
    let failed = Failed {
        error: &error,
        attempt: 0,
    };
    match rig.registry.hooks().request_error(&cx.point(), failed) {
        Asked::None => None,
        Asked::Now(by, _) => Some((by.to_string(), false)),
        Asked::Later { by, .. } => Some((by.to_string(), true)),
    }
}

/// Compaction owns the overflow and retry what may pass, so which answers
/// does not depend on the order they are installed in.
#[test]
fn compact_wins_overflow_and_retry_wins_429_in_any_order() {
    let retry = ext_retry::Retry::default();
    let orders = [
        Registry::default()
            .with_in(Tier::Pre, &Compact::default())
            .and_then(|r| r.with_in(Tier::Post, &retry)),
        Registry::default()
            .with_in(Tier::Pre, &retry)
            .and_then(|r| r.with_in(Tier::Post, &Compact::default())),
    ];
    for registry in orders {
        let rig = Rig::new("compact-order", registry.unwrap());
        let overflow = answer(&rig, llm::Kind::ContextOverflow);
        assert_eq!(overflow, Some(("compact".into(), true)), "compacted, later");
        let limited = answer(&rig, llm::Kind::RateLimit);
        assert_eq!(limited, Some(("retry".into(), false)));
        assert_eq!(answer(&rig, llm::Kind::Auth), None);
    }
}

/// A call is compacted once: if it overflows again, nothing answers, and
/// the turn fails with the provider's error.
#[test]
fn a_second_overflow_on_the_same_call_fails_the_turn() {
    let registry = Registry::default()
        .with_in(Tier::Pre, &Compact::default())
        .unwrap();
    let rig = Rig::new("compact-twice", registry);
    let cx = rig.cx();
    rt::block_on(cx.update::<Compacted>(&cx.call.get())).unwrap();
    let error = llm::Error::new(llm::Kind::ContextOverflow, "too long");
    let failed = Failed {
        error: &error,
        attempt: 1,
    };
    let asked = rig.registry.hooks().request_error(&cx.point(), failed);
    assert!(matches!(asked, Asked::None));
}

/// With a threshold, a request estimated past that share of its model's
/// window fails as an overflow before it is sent; under it, or for a model
/// with no known window, it goes as it is.
#[test]
fn a_request_past_its_models_window_share_overflows_before_it_is_sent() {
    let registry = Registry::default()
        .with_in(
            Tier::Pre,
            &Compact {
                threshold: Percent::new(50),
            },
        )
        .unwrap();
    let rig = Rig::new("compact-threshold", registry);
    let cx = rig.cx();
    let sized = |model: &str, tokens: usize| llm::Request {
        model: model.into(),
        messages: [llm::Message::user("x".repeat(tokens * 4))]
            .into_iter()
            .collect(),
        ..llm::Request::default()
    };
    let stopped = |request: llm::Request| {
        let (_, answered) = rig.registry.hooks().step(&cx.point(), &mut request.clone());
        answered.map(|(by, result)| (by.to_string(), result.map(|_| ()).map_err(|e| e.kind)))
    };
    assert_eq!(
        stopped(sized("claude-sonnet-4-5", 100_001)),
        Some(("compact".into(), Err(llm::Kind::ContextOverflow)))
    );
    assert_eq!(stopped(sized("claude-sonnet-4-5", 99_000)), None);
    assert_eq!(stopped(sized("my-deployment", 10_000_000)), None);
}

/// A threshold is a share of a window, never a token count.
#[test]
fn a_threshold_outside_one_to_a_hundred_percent_is_refused() {
    assert_eq!(Percent::new(0), None);
    assert_eq!(Percent::new(101), None);
    assert_eq!(Percent::new(80).unwrap().of(200_000), 160_000);
}

/// Answers every turn with a summary, keeping the prompts it was asked.
struct Summarizer(std::sync::Mutex<Vec<String>>);

impl ext::Conversations for Summarizer {
    fn turn<'a>(
        &'a self,
        _: &'a Cx,
        turn: ext::ModelTurn,
    ) -> ext::BoxFuture<'a, Result<llm::Response, llm::Error>> {
        let asked = llm::Json::encode(&turn.messages).unwrap().raw().to_owned();
        self.0.lock().unwrap().push(asked);
        Box::pin(std::future::ready(Ok(llm::Response {
            message: llm::Assistant {
                content: vec![llm::AssistantPart::Text(llm::Text::new("the summary"))],
                origin: None,
            },
            finish: llm::Finish::new(llm::Reason::Stop, None),
            usage: llm::Usage::default(),
            id: None,
            model: None,
            meta: llm::Meta::new(),
        })))
    }
}

struct Summaries(std::sync::Arc<Summarizer>);

impl Extension for Summaries {
    const NAME: &'static str = "summaries";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn ext::Conversations>(self.0.clone());
    }
}

/// `/compact <instructions>` asks the summary to keep what the user says,
/// and plain `/compact` asks the summary alone.
#[test]
fn compact_instructions_reach_the_summary_prompt() {
    let summarizer = std::sync::Arc::new(Summarizer(std::sync::Mutex::default()));
    let registry = Registry::default()
        .with(&Summaries(summarizer.clone()))
        .and_then(|r| r.with(&Compact::default()))
        .unwrap();
    let rig = Rig::new("compact-instructions", registry);
    let _ = (rig.cx(), rig.cx());
    for args in ["keep every API name", ""] {
        let summary = rt::block_on(CompactNow.call(&Args { args: args.into() }, &rig.cx()));
        assert_eq!(summary.as_deref(), Ok("the summary"));
    }
    let asked = summarizer.0.lock().unwrap();
    assert!(asked[0].contains("The user's instructions for this summary: keep every API name"));
    assert!(asked[1].contains("Leave out nothing a continuation needs"));
    assert!(!asked[1].contains("instructions for this summary"));
}
