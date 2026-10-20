use serde_json::json;

use super::*;
use crate::protocol::{Auth, Provider};
use crate::protocols::{chat, gemini, messages, responses};
use crate::spec::{
    Assistant, AssistantPart, Output, Request, Text, ToolCall, ToolMessage, ToolResult,
};

fn providers() -> [Provider; 4] {
    let url = "https://example.test";
    [
        Provider::new("openai", url, Auth::None, chat::Chat::new(chat::HOSTS[0].1)),
        Provider::new(
            "anthropic",
            url,
            Auth::None,
            messages::Messages::new(messages::HOSTS[0].1),
        ),
        Provider::new(
            "openai",
            url,
            Auth::None,
            responses::Responses::new(responses::HOSTS[0].1),
        ),
        Provider::new(
            "gemini",
            url,
            Auth::None,
            gemini::Gemini::new(gemini::HOSTS[0].1),
        ),
    ]
}

/// A tool round `provider` produced for `model`: its call and the result.
fn round(provider: &Provider, model: &str, turn: usize) -> [Message; 2] {
    let id: SmolStr = format!("call_{turn}").into();
    let call = ToolCall {
        id: id.clone(),
        name: "bash".into(),
        input: crate::Json::encode(&json!({"command": "ls"})).unwrap(),
        invalid: None,
        provider_executed: false,
        meta: crate::Meta::new(),
    };
    let reply = Assistant {
        content: vec![
            AssistantPart::Text(Text::new(format!("step {turn}"))),
            AssistantPart::ToolCall(call),
        ],
        origin: Some(crate::origin(provider, model)),
    };
    let result = ToolResult {
        id,
        name: "bash".into(),
        output: Output::Text(format!("file {turn}\n").repeat(100)),
        error: false,
        meta: crate::Meta::new(),
    };
    [
        Message::Assistant(reply),
        Message::Tool(ToolMessage {
            results: vec![result],
        }),
    ]
}

fn body(provider: &Provider, messages: &Messages) -> Vec<u8> {
    let request = Request {
        model: "claude-sonnet-4-5".into(),
        messages: messages.clone(),
        generation: crate::Generation {
            max_output_tokens: std::num::NonZeroU32::new(64),
            ..crate::Generation::default()
        },
        ..Request::default()
    };
    let sent = provider.request(&request.model, &request);
    sent.expect("the request encodes").body().to_vec()
}

fn folded() -> usize {
    FOLDED.with(std::cell::Cell::take)
}

#[test]
fn building_a_request_reserializes_nothing() {
    for provider in providers() {
        let mut messages = Messages::from(vec![Message::system("terse"), Message::user("go")]);
        body(&provider, &messages);
        assert_eq!(folded(), 2);
        for turn in 0..20 {
            for message in round(&provider, "claude-sonnet-4-5", turn) {
                messages.push(message);
            }
            let sent = body(&provider, &messages);
            // Only the messages added since the last request are encoded.
            assert_eq!(folded(), 2, "{}", provider.protocol.key());
            let whole = body(&provider, &Messages::from(messages.to_vec()));
            assert_eq!(folded(), messages.len());
            assert_eq!(sent, whole, "{}", provider.protocol.key());
        }
        // The same conversation again encodes nothing.
        body(&provider, &messages);
        assert_eq!(folded(), 0);
    }
}

#[test]
fn a_change_before_the_end_is_encoded_again() {
    for provider in providers() {
        let mut messages = Messages::from(vec![Message::system("terse"), Message::user("go")]);
        for message in round(&provider, "claude-sonnet-4-5", 0) {
            messages.push(message);
        }
        body(&provider, &messages);
        folded();
        // The last message changed: folded again from the start.
        if let Some(Message::Tool(tool)) = messages.last_mut() {
            tool.results[0].output = Output::Text("changed".into());
        }
        let sent = body(&provider, &messages);
        assert_eq!(folded(), messages.len());
        assert_eq!(sent, body(&provider, &Messages::from(messages.to_vec())));
        folded();
        // A message inserted before the end, as a session's context is.
        messages.insert(1, Message::user("<context/>"));
        let sent = body(&provider, &messages);
        assert_eq!(folded(), messages.len());
        assert_eq!(sent, body(&provider, &Messages::from(messages.to_vec())));
        folded();
        // A copy that grows apart keeps what the shared prefix encoded.
        let mut copy = messages.clone();
        copy.push(Message::user("and then"));
        let sent = body(&provider, &copy);
        assert_eq!(folded(), 1);
        assert_eq!(sent, body(&provider, &Messages::from(copy.to_vec())));
    }
}
