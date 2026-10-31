use super::*;
use protocol::{Entry, Epoch, Seq};

#[test]
fn conversation_metadata_matches_the_admitted_request_and_time() {
    let json = crate::tests::spawn_intent(17, "frozen prompt");
    let admitted = SpawnIntent::from_json(&json).unwrap();
    let (at, request) = SpawnIntent::description(&json).unwrap();
    assert_eq!(at, admitted.admitted_at);
    assert_eq!(
        request.to_json().unwrap(),
        admitted.request.to_json().unwrap()
    );
}

#[test]
fn checking_a_new_spawn_never_decodes_settled_admissions() {
    let mut links: Vec<_> = (1..=256)
        .map(|seq| protocol::ledger::Link {
            call: CallId::new(seq).unwrap(),
            seq: Seq::new(seq).unwrap(),
            child: SessionId::new(),
            input: "child".into(),
            intent: crate::tests::spawn_intent(seq, "child"),
        })
        .collect();
    let request = SpawnIntent::from_json(&links[0].intent).unwrap().request;
    let call = CallId::new(1000).unwrap();
    let mut admissions = Admissions::default();
    SPAWN_READS.with(std::cell::Cell::take);
    assert!(admissions.find(&links, call, &request).is_none());
    assert_eq!(SPAWN_READS.with(std::cell::Cell::take), 256);
    assert!(admissions.find(&links, call, &request).is_none());
    assert!(admissions.last(&links, call).is_none());
    assert_eq!(SPAWN_READS.with(std::cell::Cell::take), 0);
    links.push(protocol::ledger::Link {
        call,
        seq: Seq::new(257).unwrap(),
        child: SessionId::new(),
        input: "new".into(),
        intent: crate::tests::spawn_intent(call.get(), "new"),
    });
    assert_eq!(admissions.find(&links, call, &request), links.last());
    assert_eq!(admissions.last(&links, call), links.last());
    assert_eq!(SPAWN_READS.with(std::cell::Cell::take), 1);
    assert_eq!(Admissions::default().last(&links, call), links.last());
}

#[test]
fn normalization_never_changes_frozen_settings_or_authority() {
    let original = crate::tests::spawn_intent(17, "frozen prompt");
    let before = protocol::publication::SpawnImage::from_json(&original).unwrap();
    let mut intent = SpawnIntent::from_json(&original).unwrap();
    intent.image = crate::context::test_append(&[Entry::new(
        Seq::new(1).unwrap(),
        Epoch(0),
        Body::Assistant {
            text: "context".into(),
        },
    )]);
    let after = protocol::publication::SpawnImage::from_json(&intent.into_json().unwrap()).unwrap();
    assert_eq!(before.authority, after.authority);
    let restored = SpawnIntent::from_json(&after.to_json().unwrap()).unwrap();
    assert_eq!(restored.call, CallId::new(17).unwrap());
    assert_eq!(restored.request.prompt, "frozen prompt");
    assert_eq!(restored.request.depth.remaining(), 7);
}
