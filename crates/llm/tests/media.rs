//! Images in tool results, through every codec: bytes the caller holds go
//! as the base64 data they are, written only into the body; a model that
//! reads images inside a result gets them there, and any other gets them in
//! a user message after the results, tagged with the call they answer.

use std::sync::Arc;

use llm::protocols::{chat, gemini, messages, responses};
use llm::{
    AssistantPart, Media, Message, Meta, Output, Provider, Request, Source, Text, ToolCall,
    ToolMessage, ToolResult, User, UserPart,
};
use serde_json::{Value, json};

/// A PNG's signature and header, enough to stand for one.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUg==";

fn conversation(source: Source) -> Request {
    let call = ToolCall {
        id: "call_1".into(),
        name: "read".into(),
        input: llm::Json::encode(&json!({"path": "shot.png"})).unwrap(),
        invalid: None,
        provider_executed: false,
        meta: Meta::new(),
    };
    let result = ToolResult {
        id: "call_1".into(),
        name: "read".into(),
        output: Output::Content(vec![
            UserPart::Text(Text::new("shot.png")),
            UserPart::Media(Media {
                media_type: "image/png".into(),
                source,
                meta: Meta::new(),
            }),
        ]),
        error: false,
        meta: Meta::new(),
    };
    let tool = llm::Tool {
        name: "read".into(),
        description: String::new(),
        schema: json!({"type": "object"}),
        strict: None,
    };
    Request {
        model: String::new().into(),
        messages: vec![
            Message::User(User {
                content: vec![UserPart::Text(Text::new("look"))],
            }),
            Message::Assistant(llm::Assistant {
                content: vec![AssistantPart::ToolCall(call)],
                origin: None,
            }),
            Message::Tool(ToolMessage {
                results: vec![result],
            }),
        ]
        .into(),
        tools: vec![tool].into(),
        ..Request::default()
    }
}

fn held() -> Request {
    conversation(Source::Bytes {
        data: Arc::from(PNG),
    })
}

fn body(provider: &Provider, model: &str, request: &Request) -> Value {
    let mut request = request.clone();
    request.model = model.into();
    let http = provider
        .request(model, &request)
        .expect("the request encodes");
    serde_json::from_slice(&http.body().to_vec()).expect("a JSON body")
}

fn targets() -> Vec<(Provider, &'static str)> {
    let url = "http://local";
    let auth = || llm::Auth::None;
    let chat_at = |id: &str| {
        let (host, quirks) = (chat::HOSTS.iter())
            .find(|(host, _)| host.id == id)
            .expect("a chat host");
        Provider::new(host.id, url, auth(), chat::Chat::new(*quirks))
    };
    vec![
        (chat_at("openai"), "gpt-4o"),
        (chat_at("llamacpp"), "Qwen3-VL-2B-Instruct-Q8_0"),
        (
            Provider::new(
                "anthropic",
                url,
                auth(),
                messages::Messages::new(messages::HOSTS[0].1),
            ),
            "claude-sonnet-4-5",
        ),
        (
            Provider::new(
                "openai",
                url,
                auth(),
                responses::Responses::new(responses::HOSTS[0].1),
            ),
            "gpt-5",
        ),
        (
            Provider::new(
                "gemini",
                url,
                auth(),
                gemini::Gemini::new(gemini::HOSTS[0].1),
            ),
            "gemini-3-pro-preview",
        ),
        (
            Provider::new(
                "gemini",
                url,
                auth(),
                gemini::Gemini::new(gemini::HOSTS[0].1),
            ),
            "gemini-2.5-flash",
        ),
    ]
}

#[test]
fn held_bytes_are_sent_as_the_base64_data_they_are() {
    let data = conversation(Source::Data {
        data: PNG_BASE64.into(),
    });
    for (provider, model) in targets() {
        let sent = body(&provider, model, &held());
        assert_eq!(sent, body(&provider, model, &data), "{model}");
        assert!(sent.to_string().contains(PNG_BASE64), "{model}: {sent}");
    }
}

/// The tag a moved image follows, naming its call.
const TAG: &str = r#"<tool-result-media call="call_1" tool="read"/>"#;

#[test]
fn a_model_that_reads_images_in_results_gets_them_there() {
    let [_, llamacpp, anthropic, responses, gemini3, _] =
        <[_; 6]>::try_from(targets()).ok().unwrap();

    let sent = body(&anthropic.0, anthropic.1, &held());
    let result = &sent["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(
        result["content"][1],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG_BASE64}})
    );

    let sent = body(&responses.0, responses.1, &held());
    let output = (sent["input"].as_array().unwrap().iter())
        .find(|item| item["type"] == "function_call_output")
        .expect("the call's output");
    assert_eq!(output["output"][1]["type"], "input_image");
    assert_eq!(
        output["output"][1]["image_url"],
        format!("data:image/png;base64,{PNG_BASE64}")
    );

    let sent = body(&gemini3.0, gemini3.1, &held());
    let response = &sent["contents"][2]["parts"][0]["functionResponse"];
    assert_eq!(
        response["parts"][0]["inlineData"],
        json!({"mimeType": "image/png", "data": PNG_BASE64})
    );

    let sent = body(&llamacpp.0, llamacpp.1, &held());
    let tool = &sent["messages"][2];
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["content"][1]["type"], "image_url");

    for (provider, model) in [llamacpp, anthropic, responses, gemini3] {
        let sent = body(&provider, model, &held()).to_string();
        assert!(!sent.contains("tool-result-media"), "{model}: {sent}");
    }
}

#[test]
fn a_model_that_reads_none_in_results_gets_them_after_a_tag_naming_the_call() {
    let [openai, _, _, _, _, gemini2] = <[_; 6]>::try_from(targets()).ok().unwrap();

    let sent = body(&openai.0, openai.1, &held());
    assert_eq!(sent["messages"][2]["role"], "tool");
    assert!(
        !sent["messages"][2].to_string().contains("image_url"),
        "{sent}"
    );
    assert_eq!(
        sent["messages"][3],
        json!({"role": "user", "content": [
            {"type": "text", "text": TAG},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PNG_BASE64}"), "detail": "auto"}},
        ]})
    );

    let sent = body(&gemini2.0, gemini2.1, &held());
    let response = &sent["contents"][2]["parts"][0]["functionResponse"];
    assert!(response.get("parts").is_none(), "{response}");
    assert_eq!(
        sent["contents"][3]["parts"],
        json!([
            {"text": TAG, "thought": false},
            {"inlineData": {"mimeType": "image/png", "data": PNG_BASE64}, "thought": false},
        ])
    );
}

/// A conversation of `images` tool results, each holding one image of
/// `bytes` bytes, oldest first.
fn many(images: usize, bytes: usize) -> Request {
    let mut request = Request::default();
    let mut messages = vec![Message::user("look")];
    for n in 0..images {
        let id: llm::SmolStr = format!("call_{n}").into();
        messages.push(Message::Assistant(llm::Assistant {
            content: vec![AssistantPart::ToolCall(ToolCall {
                id: id.clone(),
                name: "read".into(),
                input: llm::Json::encode(&json!({})).unwrap(),
                invalid: None,
                provider_executed: false,
                meta: Meta::new(),
            })],
            origin: None,
        }));
        messages.push(Message::Tool(ToolMessage {
            results: vec![ToolResult {
                id,
                name: "read".into(),
                output: Output::Content(vec![UserPart::Media(Media {
                    media_type: "image/png".into(),
                    source: Source::Bytes {
                        data: vec![u8::try_from(n % 256).unwrap(); bytes].into(),
                    },
                    meta: Meta::new(),
                })]),
                error: false,
                meta: Meta::new(),
            }],
        }));
    }
    request.messages = messages.into();
    request
}

/// Each provider refuses a request past its size or image count: one
/// that would be sends its newest images, and says where it left the
/// older ones out.
#[test]
fn a_request_past_its_providers_limits_sends_its_newest_images() {
    let left_out = |sent: &Value| sent.to_string().matches(llm::fit::IMAGE_LEFT_OUT).count();
    let [_, _, anthropic, _, gemini, _] = <[_; 6]>::try_from(targets()).ok().unwrap();

    // Three of 6 MB, 8 MB as base64: Gemini takes 20 MB a request.
    let sent = body(&gemini.0, gemini.1, &many(3, 6_000_000));
    assert_eq!(
        left_out(&sent),
        1,
        "the newest two fit with the rest of the body"
    );
    let parts = sent["contents"].to_string();
    let newest = &base64_of(&vec![2; 6_000_000]);
    assert!(parts.contains(newest.as_str()));

    // 101 small ones: a 200k-window Claude takes 100.
    let sent = body(&anthropic.0, anthropic.1, &many(101, 16));
    assert_eq!(left_out(&sent), 1);
    let first = &sent["messages"][2]["content"][0]["content"][0];
    assert_eq!(first["text"], llm::fit::IMAGE_LEFT_OUT, "the oldest");

    let sent = body(&anthropic.0, anthropic.1, &many(100, 16));
    assert_eq!(left_out(&sent), 0, "within both, all are sent");

    for (provider, model) in targets()
        .into_iter()
        .filter(|(provider, _)| provider.id == "openai")
    {
        let sent = body(&provider, model, &many(1_500, 16));
        assert_eq!(left_out(&sent), 0, "OpenAI accepts 1,500 images");
        let sent = body(&provider, model, &many(1_501, 16));
        assert_eq!(
            left_out(&sent),
            1,
            "OpenAI leaves out the oldest excess image"
        );
    }
}

fn base64_of(bytes: &[u8]) -> String {
    let request = conversation(Source::Bytes {
        data: Arc::from(bytes),
    });
    let (provider, model) = targets().swap_remove(4);
    let sent = body(&provider, model, &request);
    sent["contents"][2]["parts"][0]["functionResponse"]["parts"][0]["inlineData"]["data"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn images_share_the_actual_encoded_body_budget_with_text_and_tools() {
    for (provider, model) in targets() {
        let mut request = many(2, 1024);
        request.model = model.into();
        let text = "\"\\\n".repeat(300);
        request
            .messages
            .canonical_mut()
            .unwrap()
            .insert(0, Message::system(&text));
        request.tools = vec![llm::Tool {
            name: "read".into(),
            description: text.clone(),
            schema: json!({"type": "object"}),
            strict: None,
        }]
        .into();
        let target = llm::protocol::Target {
            provider: &provider,
            model,
        };
        let encode = |request: &Request| provider.protocol.encode(&target, request);
        let full = encode(&request).unwrap().payload();
        let (_, fitted) = llm::fit::fit(
            &request,
            llm::fit::Limits {
                bytes: full.len() as u64,
                images: usize::MAX,
            },
            encode,
        )
        .unwrap();
        assert_eq!(
            fitted.to_vec(),
            full.to_vec(),
            "a body exactly at the cap keeps every image"
        );
        let (_, fitted) = llm::fit::fit(
            &request,
            llm::fit::Limits {
                bytes: full.len() as u64 - 1,
                images: usize::MAX,
            },
            encode,
        )
        .unwrap();
        assert!(fitted.len() < full.len(), "{}", provider.id);
        let body: Value = serde_json::from_slice(&fitted.to_vec()).unwrap();
        assert_eq!(
            body.to_string().matches(llm::fit::IMAGE_LEFT_OUT).count(),
            1,
            "{}",
            provider.id
        );
        assert_eq!(
            encode(&request).unwrap().payload().to_vec(),
            full.to_vec(),
            "fitting leaves the conversation intact"
        );
    }
}

#[test]
fn body_overlays_consume_the_image_budget_without_cutting_text() {
    let (provider, model) = targets().swap_remove(4);
    let mut request = many(1, 750_000);
    request.model = model.into();
    request.body = Some(serde_json::Map::from_iter([(
        "padding".into(),
        Value::String("x".repeat(19_500_000)),
    )]));
    let sent = provider.request(model, &request).unwrap();
    assert!(sent.body().len() <= 20_000_000);
    let body: Value = serde_json::from_slice(&sent.body().to_vec()).unwrap();
    assert_eq!(body["padding"].as_str().unwrap().len(), 19_500_000);
    assert_eq!(
        body.to_string().matches(llm::fit::IMAGE_LEFT_OUT).count(),
        1
    );
}

#[test]
fn parallel_results_precede_relocated_media_even_when_delivered_separately() {
    for split in [false, true] {
        let mut request = held();
        let Message::Assistant(assistant) = request
            .messages
            .canonical_mut()
            .unwrap()
            .get_mut(1)
            .unwrap()
        else {
            panic!()
        };
        let mut second = assistant.content[0].clone();
        if let AssistantPart::ToolCall(call) = &mut second {
            call.id = "call_2".into();
        }
        assistant.content.push(second);
        let Message::Tool(tool) = request
            .messages
            .canonical_mut()
            .unwrap()
            .get_mut(2)
            .unwrap()
        else {
            panic!()
        };
        let mut second = tool.results[0].clone();
        second.id = "call_2".into();
        if split {
            request.messages.push(Message::Tool(ToolMessage {
                results: vec![second],
            }));
        } else {
            tool.results.push(second);
        }
        let targets = targets();
        for (provider, model) in [&targets[0], &targets[5]] {
            let sent = body(provider, model, &request);
            if provider.id.as_str() == "openai" {
                let messages = sent["messages"].as_array().unwrap();
                assert_eq!(messages[2]["role"], "tool");
                assert_eq!(messages[3]["role"], "tool");
                assert_eq!(messages[4]["role"], "user");
            } else {
                let contents = sent["contents"].as_array().unwrap();
                let responses = contents[2]["parts"].as_array().unwrap();
                assert_eq!(responses.len(), 2);
                assert!(
                    responses
                        .iter()
                        .all(|part| part.get("functionResponse").is_some())
                );
                assert!(contents[3].to_string().contains(PNG_BASE64));
            }
        }
    }
}

#[test]
fn failures_keep_their_context_and_media_on_wires_without_an_error_field() {
    for output in [
        Output::Text("permission denied".into()),
        Output::Json(llm::Json::encode(&json!({"reason": "permission denied"})).unwrap()),
        Output::Content(vec![]),
        {
            let request = held();
            let Message::Tool(tool) = &request.messages.canonical().unwrap()[2] else {
                panic!()
            };
            tool.results[0].output.clone()
        },
    ] {
        let mut request = held();
        let Message::Tool(tool) = request
            .messages
            .canonical_mut()
            .unwrap()
            .get_mut(2)
            .unwrap()
        else {
            panic!()
        };
        tool.results[0].error = true;
        tool.results[0].output = output.clone();
        let targets = targets();
        for (provider, model) in [&targets[0], &targets[3]] {
            let sent = body(provider, model, &request).to_string();
            assert!(sent.contains("[tool error]"), "{model}: {sent}");
            match &output {
                Output::Text(_) | Output::Json(_) => assert!(sent.contains("permission denied")),
                Output::Content(parts) if !parts.is_empty() => assert!(sent.contains(PNG_BASE64)),
                Output::Content(_) => {}
            }
        }
    }
}
