use serde_json::{Value, json};

use super::*;
use crate::protocol::{Auth, Provider};
use crate::spec::{Assistant, AssistantPart, Message, Meta, Origin, Reasoning, Text};

fn body(messages: Vec<Message>) -> Value {
    let provider = Provider::new(
        "openai",
        "https://example.test",
        Auth::None,
        Responses::default(),
    );
    let request = Request {
        model: "gpt-6-luna".into(),
        messages,
        ..Request::default()
    };
    let target = Target {
        provider: &provider,
        model: "gpt-6-luna",
    };
    let request = crate::prepare(&request, &target);
    encode::body(&Responses::default(), &target, &request).unwrap()
}

fn origin() -> Origin {
    Origin {
        protocol: KEY.into(),
        provider: "openai".into(),
        model: "gpt-6-luna".into(),
    }
}

/// A reasoning part and a text part as the decoder stores them.
fn native_turn(text: &str) -> Message {
    let reasoning = Reasoning::default();
    let fp = crate::replay::fingerprint(crate::replay::Canon::Reasoning("", false));
    let text_fp = crate::replay::fingerprint(crate::replay::Canon::Text(text));
    Message::Assistant(Assistant {
        content: vec![
            AssistantPart::Reasoning(Reasoning {
                meta: Meta::from([(
                    KEY.into(),
                    json!({"fp": fp, "item": {"type": "reasoning", "id": "rs_1", "status": null, "summary": [], "encrypted_content": "cipher"}}),
                )]),
                ..reasoning
            }),
            AssistantPart::Text(Text {
                text: text.into(),
                meta: Meta::from([(
                    KEY.into(),
                    json!({"fp": text_fp, "item": {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": text, "annotations": []}]}}),
                )]),
            }),
        ],
        origin: Some(origin()),
    })
}

#[test]
fn a_native_turn_replays_its_items_without_null_fields() {
    let body = body(vec![
        Message::user("hi"),
        native_turn("hello"),
        Message::user("again"),
    ]);
    let input = body["input"].as_array().unwrap();
    assert_eq!(
        input[1],
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "cipher"})
    );
    assert_eq!(input[2]["id"], "msg_1");
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn an_edited_part_keeps_only_its_identity() {
    let Message::Assistant(mut turn) = native_turn("hello") else {
        unreachable!()
    };
    if let AssistantPart::Text(text) = &mut turn.content[1] {
        text.text = "edited".into();
    }
    let body = body(vec![
        Message::user("hi"),
        Message::Assistant(turn),
        Message::user("again"),
    ]);
    let message = &body["input"][2];
    assert_eq!(message["id"], "msg_1");
    assert_eq!(message["content"][0]["text"], "edited");
}

#[test]
fn another_models_turn_is_rebuilt_from_text_alone() {
    let Message::Assistant(mut turn) = native_turn("hello") else {
        unreachable!()
    };
    turn.origin = Some(Origin {
        model: "gpt-4.1".into(),
        ..origin()
    });
    let body = body(vec![
        Message::user("hi"),
        Message::Assistant(turn),
        Message::user("again"),
    ]);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "{input:?}");
    assert_eq!(input[1]["type"], "message");
    assert!(input[1]["id"].as_str().unwrap().starts_with("msg_a_"));
}

#[test]
fn leading_system_messages_become_instructions() {
    let body = body(vec![Message::system("be brief"), Message::user("hi")]);
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["input"].as_array().unwrap().len(), 1);
}

#[test]
fn reasoning_models_drop_sampling_and_ask_for_summaries() {
    let provider = Provider::new(
        "openai",
        "https://example.test",
        Auth::None,
        Responses::default(),
    );
    let mut request = Request {
        model: "gpt-6-luna".into(),
        messages: vec![Message::user("hi")],
        ..Request::default()
    };
    request.generation.temperature = Some(0.2);
    request.generation.reasoning = Some(crate::Effort::High);
    let target = Target {
        provider: &provider,
        model: "gpt-6-luna",
    };
    let body = encode::body(&Responses::default(), &target, &request).unwrap();
    assert!(body.get("temperature").is_none());
    assert_eq!(
        body["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    request.model = "gpt-4.1".into();
    let target = Target {
        provider: &provider,
        model: "gpt-4.1",
    };
    let body = encode::body(&Responses::default(), &target, &request).unwrap();
    assert_eq!(body["temperature"], 0.2);
    assert!(body.get("reasoning").is_none());
}
