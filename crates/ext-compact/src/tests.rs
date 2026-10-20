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
    cx.update::<Compacted>(cx.call);
    let error = llm::Error::new(llm::Kind::ContextOverflow, "too long");
    let failed = Failed {
        error: &error,
        attempt: 1,
    };
    let asked = rig.registry.hooks().request_error(&cx.point(), failed);
    assert!(matches!(asked, Asked::None));
}

/// With a threshold, a request estimated past it fails as an overflow
/// before it is sent; under it, it goes as it is.
#[test]
fn a_request_past_the_threshold_overflows_before_it_is_sent() {
    let registry = Registry::default()
        .with_in(
            Tier::Pre,
            &Compact {
                threshold: Some(10),
            },
        )
        .unwrap();
    let rig = Rig::new("compact-threshold", registry);
    let cx = rig.cx();
    let big = llm::Request {
        messages: [llm::Message::user("x".repeat(400))].into_iter().collect(),
        ..llm::Request::default()
    };
    let (_, answered) = rig.registry.hooks().step(&cx.point(), &mut big.clone());
    let Some((by, Err(error))) = answered else {
        panic!("not stopped")
    };
    assert_eq!((&*by, error.kind), ("compact", llm::Kind::ContextOverflow));
    let small = llm::Request::default();
    let (_, answered) = rig.registry.hooks().step(&cx.point(), &mut small.clone());
    assert!(answered.is_none());
}
