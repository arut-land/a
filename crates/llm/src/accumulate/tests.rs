use serde_json::json;

use super::*;
use crate::spec::{CallEnd, Reason};

fn finish(order: Option<Vec<u32>>) -> Event {
    Event::Finish {
        finish: Finish::new(Reason::Stop, None),
        usage: Usage::default(),
        meta: Meta::new(),
        order,
    }
}

#[test]
fn builds_parts_in_index_order_with_state_and_calls() {
    let mut acc = Accumulator::new();
    let events = [
        Event::Metadata {
            id: Some("r1".into()),
            model: None,
        },
        Event::Start {
            index: 0,
            block: Block::Reasoning,
            name: None,
        },
        Event::Start {
            index: 1,
            block: Block::ToolCall,
            name: Some("f".into()),
        },
        Event::Delta {
            index: 1,
            text: "{\"a\":".into(),
        },
        Event::Delta {
            index: 0,
            text: "think".into(),
        },
        Event::End {
            index: 0,
            redacted: false,
            text: Some("think".into()),
            call: None,
        },
        Event::Meta {
            index: 0,
            meta: Meta::from([("p".into(), json!({"sig": "s"}))]),
        },
        Event::End {
            index: 1,
            redacted: false,
            text: None,
            call: Some(CallEnd {
                id: "c1".into(),
                input: crate::Json::encode(&json!({"a": 1})).unwrap(),
                invalid: None,
                provider_executed: false,
            }),
        },
        finish(None),
    ];
    for event in events {
        acc.push(event).unwrap();
    }
    let response = acc.finish(None).unwrap();
    assert_eq!(response.id.as_deref(), Some("r1"));
    let [
        AssistantPart::Reasoning(reasoning),
        AssistantPart::ToolCall(call),
    ] = response.message.content.as_slice()
    else {
        panic!("{:?}", response.message.content)
    };
    assert_eq!(reasoning.text, "think");
    assert_eq!(reasoning.meta.get("p").unwrap()["sig"], "s");
    assert_eq!((call.id.as_str(), call.name.as_str()), ("c1", "f"));
    assert_eq!(call.input.raw(), r#"{"a":1}"#);
}

#[test]
fn a_restated_end_and_a_stated_order_win() {
    let mut acc = Accumulator::new();
    for index in [0, 1] {
        acc.push(Event::Start {
            index,
            block: Block::Text,
            name: None,
        })
        .unwrap();
        acc.push(Event::Delta {
            index,
            text: "draft".into(),
        })
        .unwrap();
        acc.push(Event::End {
            index,
            redacted: false,
            text: Some(format!("final {index}")),
            call: None,
        })
        .unwrap();
    }
    acc.push(finish(Some(vec![1, 0]))).unwrap();
    let texts: Vec<_> = acc
        .finish(None)
        .unwrap()
        .message
        .content
        .into_iter()
        .map(|part| match part {
            AssistantPart::Text(text) => text.text,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(texts, ["final 1", "final 0"]);
}

#[test]
fn rejects_events_out_of_order() {
    let mut acc = Accumulator::new();
    assert!(
        acc.push(Event::Delta {
            index: 0,
            text: "x".into()
        })
        .is_err()
    );
    acc.push(finish(None)).unwrap();
    assert!(
        acc.push(Event::Metadata {
            id: None,
            model: None
        })
        .is_err()
    );
}
