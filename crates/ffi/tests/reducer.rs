//! The facade's reducer: the shared vectors (`protocol/tests/vectors`) give
//! the same updates, state and hash through it as through
//! `protocol::view`, and a client that missed updates catches up.

use a_ffi::{Reducer, Resync, ViewChange};
use serde_json::{Value, json};

fn vectors() -> Vec<Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../protocol/tests/vectors/view.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn the_shared_vectors_fold_the_same_through_the_facade() {
    let vectors = vectors();
    assert!(vectors.len() > 1);
    for vector in vectors {
        let name = vector["name"].as_str().unwrap();
        let reducer = Reducer::new();
        let updates: Vec<_> = vector["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|msg| reducer.fold(&msg.to_string()).unwrap())
            .collect();
        let expected = vector["updates"].as_array().unwrap();
        assert_eq!(updates.len(), expected.len(), "{name}");
        for (got, want) in updates.iter().zip(expected) {
            assert_eq!(got.rev, want["rev"], "{name}");
            assert_eq!(got.seq, want["seq"], "{name}");
        }
        let state: Value = serde_json::from_str(&reducer.state_json()).unwrap();
        assert_eq!(state, vector["state"], "{name}");
        assert_eq!(reducer.snapshot().state_hash, vector["hash"], "{name}");
    }
}

fn user(seq: u64) -> String {
    json!({ "t": "entry", "seq": seq, "epoch": 0, "kind": "input", "text": format!("m{seq}"), "taken": true })
        .to_string()
}

#[test]
fn a_missed_update_is_handed_back_and_a_long_gap_gets_a_snapshot() {
    let reducer = Reducer::new();
    let attached = json!({ "t": "attached", "session": "00000000-0000-0000-0000-00000000000a", "workspace": "/w" });
    reducer.fold(&attached.to_string()).unwrap();
    for seq in 1..=3 {
        reducer.fold(&user(seq)).unwrap();
    }
    let Resync::Updates { updates } = reducer.resync(2) else {
        panic!("a recent gap replays");
    };
    assert_eq!(updates.iter().map(|u| u.rev).collect::<Vec<_>>(), [3, 4]);
    assert!(matches!(
        &updates[1].change,
        ViewChange::Added { index: 2, .. }
    ));
    assert_eq!(reducer.resync(4), Resync::Updates { updates: vec![] });

    for seq in 4..=1000 {
        reducer.fold(&user(seq)).unwrap();
    }
    let Resync::Snapshot { snapshot } = reducer.resync(2) else {
        panic!("too long a gap takes a snapshot");
    };
    assert_eq!(
        (snapshot.rev, snapshot.seq, snapshot.items.len()),
        (1001, 1000, 1000)
    );
    assert!(matches!(reducer.resync(900), Resync::Updates { updates } if updates.len() == 101));
}

/// Updates that carry big outputs are kept for a resync up to a few
/// megabytes, not a fixed count: past that a gap takes a snapshot.
#[test]
fn big_outputs_are_kept_for_a_resync_by_size() {
    let reducer = Reducer::new();
    let attached = json!({ "t": "attached", "session": "00000000-0000-0000-0000-00000000000a", "workspace": "/w" });
    reducer.fold(&attached.to_string()).unwrap();
    let output = "x".repeat(64 * 1024);
    for call in (1..=200).map(|n| n * 2 - 1) {
        let started = json!({ "t": "entry", "seq": call, "epoch": 0, "kind": "tool_call",
            "tool": "bash", "input": { "command": "yes" } });
        let done = json!({ "t": "entry", "seq": call + 1, "epoch": 0, "kind": "tool_result",
            "call": call, "outcome": { "status": "done", "output": { "exit": 0, "output": output } } });
        reducer.fold(&started.to_string()).unwrap();
        reducer.fold(&done.to_string()).unwrap();
    }
    let rev = reducer.snapshot().rev;
    assert!(matches!(reducer.resync(rev - 20), Resync::Updates { updates } if updates.len() == 20));
    assert!(matches!(reducer.resync(rev - 200), Resync::Snapshot { .. }));
}

#[test]
fn a_malformed_message_is_a_protocol_error_with_a_stable_code() {
    let reducer = Reducer::new();
    let err = reducer.fold("{\"t\": \"entry\"").unwrap_err();
    assert_eq!(err.code(), "protocol");
    // A newer daemon's kind is skipped, not an error.
    assert_eq!(reducer.fold(r#"{"t": "future_thing"}"#).unwrap(), None);
}

#[test]
fn a_titling_reply_is_the_sessions_title() {
    let reducer = Reducer::new();
    let call = json!({ "t": "entry", "seq": 1, "epoch": 0, "kind": "model_call", "model": "m", "purpose": "title" });
    let reply = json!({ "t": "entry", "seq": 2, "epoch": 0, "kind": "model_result", "call": 1,
        "outcome": { "status": "done", "output": { "message": { "content": [{ "type": "text", "text": "Fix the parser" }] } } } });
    let started = reducer.fold(&call.to_string()).unwrap().unwrap();
    assert_eq!(started.title, None);
    let titled = reducer.fold(&reply.to_string()).unwrap().unwrap();
    assert_eq!(titled.title.as_deref(), Some("Fix the parser"));
    assert!(matches!(
        titled.change,
        ViewChange::Updated { item: a_ffi::ViewItem::Reply { purpose: Some(ref p), .. }, .. } if p == "title"
    ));
    assert_eq!(reducer.snapshot().title.as_deref(), Some("Fix the parser"));
}
