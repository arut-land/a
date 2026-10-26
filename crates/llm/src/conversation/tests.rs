use serde_json::{Value, json};

use super::*;
use crate::protocol::{Auth, Provider};
use crate::protocols::{chat, gemini, messages, responses};
use crate::spec::{
    Assistant, AssistantPart, Output, Request, Text, ToolCall, ToolMessage, ToolResult, Tools,
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
        // A message changed before the end: every prefix past it is
        // encoded again, though the last message is the same.
        messages.push(Message::user("next"));
        body(&provider, &messages);
        folded();
        let Some(Message::Tool(tool)) = messages.get_mut(4) else {
            panic!("the tool message");
        };
        tool.results[0].output = Output::Text("changed again".into());
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

/// The request's tools are written from the bytes the tool set keeps, in
/// the same place and shape as the body's own field, and an escape-hatch
/// `tools` is the one sent.
#[test]
fn tools_are_spliced_as_the_bodys_field_and_an_escape_hatch_replaces_them() {
    let tools = Tools::from(vec![crate::Tool {
        name: "bash".into(),
        description: "Runs a command.".into(),
        schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        strict: None,
    }]);
    for provider in providers() {
        let request = Request {
            model: "claude-sonnet-4-5".into(),
            messages: vec![Message::user("go")].into(),
            tools: tools.clone(),
            generation: crate::Generation {
                max_output_tokens: std::num::NonZeroU32::new(64),
                ..crate::Generation::default()
            },
            ..Request::default()
        };
        let sent = |request: &Request| -> Value {
            let http = provider.request(&request.model, request).expect("encodes");
            serde_json::from_slice(&http.body().to_vec()).expect("a JSON body")
        };
        let body = sent(&request);
        let tools = body["tools"].as_array().expect("tools");
        let text = serde_json::to_string(&body).unwrap();
        assert!(
            text.contains(r#""bash""#),
            "{}: {text}",
            provider.protocol.key()
        );
        assert_eq!(tools.len(), 1, "{}: {tools:?}", provider.protocol.key());
        let again = sent(&request);
        assert_eq!(again, body, "{}", provider.protocol.key());
        let overlay = Request {
            body: json!({"tools": [{"type": "hosted"}]}).as_object().cloned(),
            ..request.clone()
        };
        assert_eq!(sent(&overlay)["tools"], json!([{"type": "hosted"}]));
    }
}

#[test]
fn complete_coverage_never_drops_an_exchange_or_an_oversized_newest_message() {
    let current = vec![Message::user("summarise all supplied context")].into();
    let policy = ContextPolicy {
        exchanges: 8,
        bytes: 65536,
    };
    let mut prefix = Vec::new();
    for n in 0..20 {
        prefix.push(Message::user(format!("request {n}")));
        prefix.push(Message::assistant(format!("answer {n}")));
    }
    let large = Message::user("newest 🦀\\\"\n".repeat(20_000));
    prefix.push(large.clone());
    let mut covered = Vec::new();
    let mut fragments = String::new();
    let mut inputs = 0;
    let mut bytes = 0;
    for chunk in policy.cover(&prefix, &current) {
        let chunk = chunk.unwrap();
        inputs += 1;
        let encoded = crate::Json::encode(&chunk).unwrap();
        bytes += encoded.raw().len();
        assert!(encoded.raw().len() <= policy.bytes as usize);
        assert_eq!(chunk.last(), current.last());
        if matches!(chunk.first(), Some(Message::System(system)) if system.text().starts_with("The next message is a consecutive fragment"))
        {
            let Message::User(user) = &chunk[1] else {
                panic!("fragment text")
            };
            let crate::UserPart::Text(text) = &user.content[0] else {
                panic!("fragment text")
            };
            fragments.push_str(&text.text);
        } else {
            covered.extend_from_slice(&chunk[..chunk.len() - 1]);
        }
    }
    assert_eq!(covered, prefix[..prefix.len() - 1]);
    assert_eq!(fragments, crate::Json::encode(&[large]).unwrap().raw());
    assert!(inputs > 3);
    eprintln!(
        "complete prefix: {} source bytes, {inputs} summary inputs, {bytes} total input bytes",
        crate::Json::encode(&prefix).unwrap().raw().len()
    );
}

#[test]
fn oversized_exchanges_keep_native_media_and_all_tool_data() {
    let media = crate::Media {
        media_type: "image/png".into(),
        source: crate::Source::Bytes {
            data: Arc::from([1, 2, 3]),
        },
        meta: crate::Meta::new(),
    };
    let messages = [Message::Tool(ToolMessage {
        results: vec![ToolResult {
            id: "call".into(),
            name: "read".into(),
            output: Output::Content(vec![
                crate::UserPart::Text(Text::new("large output".repeat(20_000))),
                crate::UserPart::Media(media.clone()),
            ]),
            error: false,
            meta: crate::Meta::new(),
        }],
    })];
    let current = vec![Message::user("summary")].into();
    let chunks: Vec<_> = ContextPolicy {
        exchanges: 8,
        bytes: 65536,
    }
    .cover(&messages, &current)
    .collect::<Result<_, _>>()
    .unwrap();
    assert!(chunks.iter().any(|chunk| chunk.iter().any(|m| matches!(m,
        Message::User(user) if user.content.contains(&crate::UserPart::Media(media.clone()))))));
    let text: String = chunks
        .iter()
        .filter_map(|chunk| match chunk.first() {
            Some(Message::System(_)) => match &chunk[1] {
                Message::User(user) => match &user.content[0] {
                    crate::UserPart::Text(text) => Some(text.text.as_str()),
                    crate::UserPart::Media(_) => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect();
    let restored = Messages::from_json(&text).unwrap();
    let Message::Tool(tool) = &restored[0] else {
        panic!("tool result preserved")
    };
    assert_eq!(tool.results[0].id, "call");
    let Output::Content(content) = &tool.results[0].output else {
        panic!("tool content preserved")
    };
    assert_eq!(
        content[0],
        crate::UserPart::Text(Text::new("large output".repeat(20_000)))
    );
    assert_eq!(
        content[1],
        crate::UserPart::Text(Text::new("[media supplied separately]"))
    );
}

#[test]
fn serialized_summary_fragments_never_expose_private_provider_replay_data() {
    let mut meta = crate::Meta::new();
    meta.insert(
        "responses",
        &json!({"encrypted_content": "private-ciphertext"}),
    );
    let prefix = [Message::Assistant(Assistant {
        content: vec![AssistantPart::Reasoning(crate::Reasoning {
            text: "visible reasoning ".repeat(10_000),
            redacted: false,
            meta,
        })],
        origin: None,
    })];
    let current = vec![Message::user("summary")].into();
    let mut fragments = String::new();
    for chunk in (ContextPolicy {
        exchanges: 8,
        bytes: 65536,
    })
    .cover(&prefix, &current)
    {
        let chunk = chunk.unwrap();
        let Message::User(user) = &chunk[1] else {
            panic!("serialized fragment")
        };
        let crate::UserPart::Text(text) = &user.content[0] else {
            panic!("serialized text")
        };
        fragments.push_str(&text.text);
    }
    assert!(!fragments.contains("private-ciphertext"));
    let restored = Messages::from_json(&fragments).unwrap();
    let Message::Assistant(reply) = &restored[0] else {
        panic!("assistant context")
    };
    let AssistantPart::Reasoning(reasoning) = &reply.content[0] else {
        panic!("visible reasoning")
    };
    assert_eq!(reasoning.text, "visible reasoning ".repeat(10_000));
    let Message::Assistant(original) = &prefix[0] else {
        panic!("original assistant")
    };
    assert!(original.content[0].meta().get("responses").is_some());
}
