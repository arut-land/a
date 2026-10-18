//! The fragment merge, one rule at a time (rig's `dto` tests).

use serde_json::{Map, Value, json};

use crate::protocols::chat::merge::merge_fields;

/// `target` after merging each of `deltas` into it in turn.
fn merged(target: Value, deltas: &[Value]) -> Value {
    let Value::Object(mut target) = target else {
        panic!("the target is an object: {target}");
    };
    for delta in deltas {
        let Value::Object(delta) = delta else {
            panic!("a delta is an object: {delta}");
        };
        merge_fields(&mut target, delta);
    }
    Value::Object(target)
}

#[test]
fn fragments_append_and_identifiers_are_restated() {
    let call = merged(
        json!({}),
        &[
            json!({"id": "call_1", "arguments": "{\"a\""}),
            json!({"id": "call_1", "arguments": ":1}"}),
        ],
    );
    assert_eq!(call, json!({"id": "call_1", "arguments": "{\"a\":1}"}));
    // Blank text does not displace a `null` placeholder; the first real
    // fragment does.
    let call = merged(
        json!({"arguments": "null"}),
        &[json!({"arguments": " "}), json!({"arguments": "{}"})],
    );
    assert_eq!(call, json!({"arguments": "{}"}));
    let item = merged(
        json!({"id": "call_1", "name": "lookup"}),
        &[json!({"id": "", "name": null})],
    );
    assert_eq!(item, json!({"id": "call_1", "name": "lookup"}));
    let item = merged(json!({"type": "function"}), &[json!({"type": "custom"})]);
    assert_eq!(item, json!({"type": "custom"}));
}

#[test]
fn arrays_extend_objects_merge_and_types_never_change() {
    let item = merged(
        json!({"annotations": [1], "function": {"name": "f", "arguments": "{"}}),
        &[json!({"annotations": [2], "function": {"arguments": "}"}})],
    );
    assert_eq!(
        item,
        json!({"annotations": [1, 2], "function": {"name": "f", "arguments": "{}"}})
    );
    let item = merged(
        json!({"index": 0, "extra": null}),
        &[json!({"index": "zero", "extra": {"k": 1}})],
    );
    assert_eq!(item, json!({"index": 0, "extra": {"k": 1}}));
    let mut target = Map::new();
    merge_fields(
        &mut target,
        &Map::from_iter([("refusal".to_owned(), Value::Null)]),
    );
    assert_eq!(Value::Object(target), json!({"refusal": null}));
}

#[test]
fn content_becomes_parts_once_either_side_is_a_part_array() {
    let message = merged(
        json!({"content": "Hel"}),
        &[
            json!({"content": [{"type": "text", "text": "lo"}]}),
            json!({"content": [{"type": "image_url", "image_url": {"url": "u"}}]}),
            json!({"content": "!"}),
        ],
    );
    assert_eq!(
        message,
        json!({"content": [
            {"type": "text", "text": "Hello"},
            {"type": "image_url", "image_url": {"url": "u"}},
            {"type": "text", "text": "!"},
        ]})
    );
    let message = merged(
        json!({"content": ""}),
        &[
            json!({"content": [{"type": "text", "text": "a"}]}),
            json!({"content": [7]}),
        ],
    );
    assert_eq!(
        message,
        json!({"content": [{"type": "text", "text": "a"}, 7]})
    );
    let message = merged(json!({"content": 3}), &[json!({"content": []})]);
    assert_eq!(message, json!({"content": []}));
    let message = merged(
        json!({"content": [{"type": "thinking", "thinking": [{"type": "text", "text": "a"}]}]}),
        &[json!({"content": [
            {"type": "thinking", "thinking": [{"type": "text", "text": "b"}], "signature": "s"},
            {"type": "untyped"},
        ]})],
    );
    assert_eq!(
        message,
        json!({"content": [
            {"type": "thinking", "thinking": [{"type": "text", "text": "ab"}], "signature": "s"},
            {"type": "untyped"},
        ]})
    );
}
