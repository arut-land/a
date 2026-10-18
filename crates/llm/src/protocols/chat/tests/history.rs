//! Replay, ported from rig's Chat history tests: every part of a reply goes
//! back rebuilt from its parts, with the provider fields that belong to the
//! model that sent them, and nothing a reply only reports.

use serde_json::{Value, json};

use super::{DONE, chunk, decode, item, provider, request, sent, turn, whole};
use crate::protocols::chat::KEY;
use crate::spec::{
    Assistant, AssistantPart, Media, Message, Meta, Origin, Output, Source, ToolCall, ToolMessage,
    ToolResult, User, UserPart,
};

fn result(id: &str, name: &str, text: &str) -> Message {
    Message::Tool(ToolMessage {
        results: vec![ToolResult {
            id: id.into(),
            name: name.into(),
            output: Output::Text(text.into()),
            error: false,
            meta: Meta::new(),
        }],
    })
}

fn call(id: &str, name: &str, input: Value) -> ToolCall {
    let Value::Object(input) = input else {
        panic!("arguments are an object");
    };
    ToolCall {
        id: id.into(),
        name: name.into(),
        input,
        invalid: None,
        provider_executed: false,
        meta: Meta::new(),
    }
}

fn call_turn(id: &str, name: &str) -> Message {
    Message::Assistant(Assistant {
        content: vec![AssistantPart::ToolCall(call(id, name, json!({})))],
        origin: None,
    })
}

/// The assistant message `messages` sends second, then `next` from the
/// user, to host `id`'s `model`.
fn replayed(id: &str, model: &str, messages: Vec<Message>) -> Value {
    let mut messages = messages;
    messages.push(Message::user("next"));
    sent(&provider(id), &request(model, messages))["messages"][1].clone()
}

/// Magistral's `thinking` content parts are reasoning from both reply
/// shapes, and the same model gets them back as the parts they came as.
#[test]
fn mistral_thinking_parts_are_reasoning() {
    let thinking =
        |text: &str| json!({"type": "thinking", "thinking": [{"type": "text", "text": text}]});
    let model = "magistral-medium-latest";
    let mistral = provider("mistral");
    for frames in [
        vec![whole(
            json!({"role": "assistant", "content": [thinking("plan it"), {"type": "text", "text": "Hi"}]}),
            "stop",
        )],
        vec![
            chunk(
                json!({"role": "assistant", "content": [thinking("plan ")]}),
                None,
            ),
            chunk(json!({"content": [thinking("it")]}), None),
            chunk(json!({"content": "Hi"}), Some("stop")),
            DONE.into(),
        ],
    ] {
        let response = decode(&mistral, model, &frames);
        let [
            AssistantPart::Reasoning(reasoning),
            AssistantPart::Text(text),
        ] = response.message.content.as_slice()
        else {
            panic!("reasoning and text: {:?}", response.message.content);
        };
        assert_eq!(reasoning.text, "plan it");
        assert_eq!(text.text, "Hi");
        assert_eq!(
            replayed("mistral", model, vec![Message::user("q"), turn(&response)]),
            json!({"role": "assistant",
                "content": [thinking("plan it"), {"type": "text", "text": "Hi"}]})
        );
    }
}

/// A custom call stays a custom call in the message's `tool_calls` when a
/// sibling part is edited, when its own input is (its kind survives as
/// identity), and when another model's turn goes to a request that declares
/// the custom tool.
#[test]
fn a_custom_call_stays_custom() {
    let openai = provider("openai");
    let response = decode(
        &openai,
        "gpt-5.2",
        &[whole(
            json!({"role": "assistant", "content": "grepping now", "tool_calls": [{"id": "call_c",
                "type": "custom", "custom": {"name": "grep", "input": "rig"}}]}),
            "tool_calls",
        )],
    );
    let mut edited = response.message.clone();
    if let Some(AssistantPart::Text(text)) = edited.content.first_mut() {
        text.text = "edited".to_owned();
    }
    let custom = |input: &str| json!({"id": "call_c", "type": "custom", "custom": {"name": "grep", "input": input}});
    assert_eq!(
        replayed(
            "openai",
            "gpt-5.2",
            vec![
                Message::user("q"),
                Message::Assistant(edited),
                result("call_c", "grep", "found"),
            ],
        ),
        json!({"role": "assistant", "content": "edited", "tool_calls": [custom("rig")]})
    );

    let mut edited = response.message.clone();
    for part in &mut edited.content {
        if let AssistantPart::ToolCall(call) = part {
            call.input.insert("input".into(), json!("rig2"));
        }
    }
    let same = replayed(
        "openai",
        "gpt-5.2",
        vec![
            Message::user("q"),
            Message::Assistant(edited),
            result("call_c", "grep", "found"),
        ],
    );
    assert_eq!(same["tool_calls"][0], custom("rig2"));

    let mut declared = request(
        "gpt-5.4",
        vec![
            Message::user("q"),
            turn(&response),
            result("call_c", "grep", "found"),
            Message::user("next"),
        ],
    );
    declared.options.insert(
        KEY.into(),
        json!({"tools": [{"type": "custom", "custom": {"name": "grep", "description": "search"}}]}),
    );
    let other = sent(&openai, &declared);
    assert_eq!(other["messages"][1]["tool_calls"][0], custom("rig"));
}

/// A reply's message never goes back as it came: object-form arguments go
/// as JSON text, and a call's stream `index`, annotations and an answer's
/// audio data stay home.
#[test]
fn response_only_fields_never_go_back() {
    let model = "gpt-4o-audio-preview";
    let response = decode(
        &provider("openai"),
        model,
        &[whole(
            json!({"role": "assistant", "content": "see rig.rs",
                "annotations": [{"type": "url_citation", "url_citation": {"url": "https://rig.rs"}}],
                "audio": {"id": "audio_1", "data": "UklG", "expires_at": 1, "transcript": "see"},
                "tool_calls": [{"id": "call_1", "type": "function",
                    "function": {"name": "add", "arguments": {"x": 1}}}]}),
            "tool_calls",
        )],
    );
    assert_eq!(
        replayed(
            "openai",
            model,
            vec![
                Message::user("q"),
                turn(&response),
                result("call_1", "add", "2"),
            ],
        ),
        json!({"role": "assistant", "content": "see rig.rs", "audio": {"id": "audio_1"},
            "tool_calls": [{"id": "call_1", "type": "function",
                "function": {"name": "add", "arguments": "{\"x\":1}"}}]})
    );
}

/// A field rig has never seen on a message stays home; one on a tool call is
/// the call's own item, and goes back to the model that sent it.
#[test]
fn an_invented_call_field_replays_and_an_invented_message_field_does_not() {
    let call = json!({"id": "call_probe", "type": "function",
        "function": {"name": "add", "arguments": "{\"x\":1}"}, "x_call_probe": {"kept": true}});
    let whole_reply = vec![whole(
        json!({"role": "assistant", "content": "adding",
            "x_message_probe": [{"kind": "invented"}], "tool_calls": [call]}),
        "tool_calls",
    )];
    let streamed = vec![
        chunk(json!({"role": "assistant", "content": "add"}), None),
        chunk(
            json!({"content": "ing", "x_message_probe": [{"kind": "invented"}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_probe", "type": "function",
                "function": {"name": "add", "arguments": "{\"x\""}, "x_call_probe": {"kept": true}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": ":1}"}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        DONE.into(),
    ];
    let model = "gpt-4.1-nano";
    let openai = provider("openai");
    let unary = decode(&openai, model, &whole_reply);
    let stream = decode(&openai, model, &streamed);
    assert_eq!(unary.message.content, stream.message.content);
    let probed = unary
        .message
        .content
        .iter()
        .find_map(|part| item(part).filter(|item| item.get("x_call_probe").is_some()))
        .expect("the call keeps its item");
    assert_eq!(probed["x_call_probe"]["kept"], true);
    let replayed = replayed(
        "openai",
        model,
        vec![
            Message::user("add one"),
            turn(&unary),
            result("call_probe", "add", "1"),
        ],
    );
    assert!(replayed.get("x_message_probe").is_none(), "{replayed}");
    assert_eq!(replayed["tool_calls"][0]["x_call_probe"]["kept"], true);
    assert_eq!(replayed["content"], "adding");
}

/// Perplexity takes no tools, so a call and its result go as text the model
/// still reads, rather than being dropped.
#[test]
fn perplexity_reads_a_tool_exchange_as_text() {
    let history = vec![
        Message::user("look it up"),
        Message::Assistant(Assistant {
            content: vec![AssistantPart::ToolCall(call(
                "call_1",
                "lookup",
                json!({"q": "rig"}),
            ))],
            origin: Some(Origin {
                protocol: KEY.into(),
                provider: "openai".into(),
                model: "gpt-4.1".into(),
            }),
        }),
        result("call_1", "lookup", "crimson"),
        Message::user("next"),
    ];
    let body = sent(&provider("perplexity"), &request("sonar-pro", history));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "look it up"},
            {"role": "assistant", "content": "[called tool lookup with {\"q\":\"rig\"}]"},
            {"role": "user", "content": "[tool lookup result] crimson\nnext"},
        ])
    );
    assert!(body.get("tools").is_none(), "{body}");
}

/// The text-only models of Z.AI, Moonshot, `MiniMax` and `MiMo`, on their own
/// hosts and through `OpenRouter`, get a note for an image, and their vision
/// models still get the image.
#[test]
fn text_only_models_get_a_note_for_an_image() {
    let history = vec![Message::User(User {
        content: vec![
            UserPart::Text(crate::Text::new("look")),
            UserPart::Media(Media {
                media_type: "image/png".into(),
                source: Source::Data {
                    data: "iVBORw0KGgo=".into(),
                },
                meta: Meta::new(),
            }),
        ],
    })];
    let cases = [
        ("zai", "glm-4.6", false),
        ("zai", "glm-4.5v", true),
        ("moonshot", "kimi-k2-0905-preview", false),
        ("moonshot", "kimi-k2.6", true),
        ("minimax", "MiniMax-M2", false),
        ("minimax", "MiniMax-M3", true),
        ("xiaomimimo", "mimo-v2-flash", false),
        ("xiaomimimo", "mimo-v2.5", true),
        ("openrouter", "z-ai/glm-4.6", false),
        ("openrouter", "moonshotai/kimi-k2-0905", false),
        ("openrouter", "minimax/minimax-m2", false),
        ("openrouter", "xiaomi/mimo-v2-flash", false),
        ("openrouter", "deepseek/deepseek-chat-v3.1", false),
        ("openrouter", "z-ai/glm-4.5v", true),
    ];
    let wrong: Vec<String> = cases
        .iter()
        .filter(|(id, model, images)| {
            let body = sent(&provider(id), &request(model, history.clone())).to_string();
            body.contains("image_url") != *images
                || body.contains(crate::protocols::chat::encode::USER_IMAGE_OMITTED) == *images
        })
        .map(|(id, model, _)| format!("{id}/{model}"))
        .collect();
    assert!(wrong.is_empty(), "{wrong:?}");
}

/// An answer's audio transcript is a text part holding the audio's id, so
/// the same model gets the audio back by its id with the transcript, and
/// another model gets the transcript.
#[test]
fn an_answers_audio_transcript_is_text() {
    let model = "gpt-4o-audio-preview";
    let openai = provider("openai");
    for frames in [
        vec![whole(
            json!({"role": "assistant", "content": null, "audio": {"id": "audio_1",
                "transcript": "hello there", "data": "UklG", "expires_at": 1}}),
            "stop",
        )],
        vec![
            chunk(
                json!({"role": "assistant", "content": null,
                    "audio": {"id": "audio_1", "transcript": "hello "}}),
                None,
            ),
            chunk(
                json!({"audio": {"transcript": "there", "data": "UklG", "expires_at": 1}}),
                Some("stop"),
            ),
            DONE.into(),
        ],
    ] {
        let response = decode(&openai, model, &frames);
        let [part @ AssistantPart::Text(text)] = response.message.content.as_slice() else {
            panic!(
                "the transcript is the one part: {:?}",
                response.message.content
            );
        };
        assert_eq!(text.text, "hello there");
        assert_eq!(item(part), Some(&json!({"audio": {"id": "audio_1"}})));
        let history = vec![Message::user("hi"), turn(&response)];
        assert_eq!(
            replayed("openai", model, history.clone()),
            json!({"role": "assistant", "content": "hello there", "audio": {"id": "audio_1"}})
        );
        assert_eq!(
            replayed("openai", "gpt-4.1-mini", history),
            json!({"role": "assistant", "content": "hello there"})
        );
    }
}

/// An edited reasoning part of the same model goes back under the field it
/// arrived in: `DeepSeek` and Kimi read their reasoning back on a call turn.
#[test]
fn an_edited_reasoning_part_keeps_its_field() {
    for (id, model, call_id) in [
        ("deepseek", "deepseek-reasoner", "call_0"),
        ("moonshot", "kimi-k2-thinking", "functions.f:0"),
    ] {
        let response = decode(
            &provider(id),
            model,
            &[whole(
                json!({"role": "assistant", "content": "", "reasoning_content": "plan",
                    "tool_calls": [{"id": call_id, "type": "function",
                        "function": {"name": "f", "arguments": "{}"}}]}),
                "tool_calls",
            )],
        );
        let mut edited = response.message.clone();
        for part in &mut edited.content {
            if let AssistantPart::Reasoning(reasoning) = part {
                reasoning.text = "plan (edited)".to_owned();
            }
        }
        let replayed = replayed(
            id,
            model,
            vec![
                Message::user("q"),
                Message::Assistant(edited),
                result(call_id, "f", "ok"),
            ],
        );
        assert_eq!(
            replayed["reasoning_content"], "plan (edited)",
            "{id}: {replayed}"
        );
    }
}

/// A content part with no `type` and a string `text` is text, and nothing
/// of it lands on the replayed message's own fields.
#[test]
fn a_typeless_text_part_is_text() {
    let response = decode(
        &provider("openai"),
        "gpt-4o",
        &[whole(
            json!({"role": "assistant", "content": [{"text": "hello"}]}),
            "stop",
        )],
    );
    assert_eq!(response.message.text(), "hello");
    let assistant = replayed(
        "openai",
        "gpt-4o",
        vec![Message::user("q"), turn(&response)],
    );
    assert_eq!(assistant["content"], "hello");
    assert!(assistant.get("text").is_none(), "{assistant}");
}

/// The models that ask for it get an empty `reasoning_content` on a call
/// turn with none: `DeepSeek` at its own base URL behind the `OpenAI` host,
/// every `MiMo` model, and Kimi K3 and `OpenRouter`'s Kimi K2.6 behind any
/// gateway. Other models get none.
#[test]
fn reasoning_content_follows_the_model() {
    let history = || {
        vec![
            Message::user("q"),
            call_turn("call_0", "f"),
            result("call_0", "f", "x"),
            Message::user("next"),
        ]
    };
    let field = |provider: &crate::Provider, model: &str| {
        sent(provider, &request(model, history()))["messages"][1]
            .get("reasoning_content")
            .cloned()
    };
    let deepseek = provider("openai").with_base_url("https://api.deepseek.com");
    assert_eq!(field(&deepseek, "deepseek-reasoner"), Some(json!("")));
    for (id, model) in [
        ("deepseek", "deepseek-reasoner"),
        ("xiaomimimo", "mimo-v2.5"),
        ("moonshot", "kimi-k3"),
        ("openai", "accounts/fireworks/models/kimi-k3"),
        ("openrouter", "moonshotai/kimi-k2.6"),
        ("openrouter", "moonshotai/kimi-k3"),
        ("venice", "kimi-k3"),
        ("ollama", "kimi-k3"),
        ("together", "moonshotai/kimi-k2.6"),
    ] {
        assert_eq!(field(&provider(id), model), Some(json!("")), "{id}/{model}");
    }
    for (id, model) in [("openai", "gpt-4o"), ("moonshot", "kimi-k2.6")] {
        assert_eq!(field(&provider(id), model), None, "{id}/{model}");
    }
}

/// `reasoning_details` join the message's one reasoning part, its first,
/// wherever they arrive, so a stream folds to the turn its whole message
/// folds to and replays the same bytes: a signature alone after the
/// content (Gemini 3 through `OpenRouter`), reasoning interleaved with the
/// answer and a trailing ciphertext, reasoning text after the answer, a
/// ciphertext ahead of a call, and a signature on the first of two calls.
#[test]
fn reasoning_details_fold_alike_whole_and_streamed() {
    let model = "anthropic/claude-opus-5.5";
    let openrouter = provider("openrouter");
    let signature = json!({"type": "reasoning.text", "signature": "AY89a19Q",
        "format": "google-gemini-v1", "index": 0});
    let signed = json!({"type": "reasoning.text", "text": "plan", "signature": "EsYF",
        "format": "anthropic-claude-v1", "index": 0});
    let encrypted = json!({"type": "reasoning.encrypted", "data": "enc",
        "format": "openai-responses-v1", "index": 0, "id": "rs_1"});
    let first = json!({"id": "call_1", "type": "function",
        "function": {"name": "f", "arguments": "{}"}});
    let second = json!({"id": "call_2", "type": "function",
        "function": {"name": "f", "arguments": "{}"}});
    let indexed = |call: &Value, index: u32| {
        let mut call = call.clone();
        call["index"] = json!(index);
        call
    };
    let signs_call = json!({"type": "reasoning.encrypted", "data": "gsig",
        "format": "google-gemini-v1", "index": 0, "id": "call_1"});
    let cases = [
        (
            json!({"role": "assistant", "content": "The code is amber.",
                "reasoning_details": [signature]}),
            vec![
                json!({"role": "assistant", "content": "The code is amber."}),
                json!({"role": "assistant", "content": "", "reasoning_details": [signature]}),
            ],
            vec!["reasoning", "text"],
        ),
        (
            json!({"role": "assistant", "content": "answer", "reasoning": "plan",
                "reasoning_details": [signed, encrypted]}),
            vec![
                json!({"role": "assistant", "reasoning": "plan", "reasoning_details": [signed]}),
                json!({"content": "answer"}),
                json!({"reasoning_details": [encrypted]}),
            ],
            vec!["reasoning", "text"],
        ),
        (
            json!({"role": "assistant", "content": "ab", "reasoning": "xy"}),
            vec![
                json!({"role": "assistant", "reasoning": "x"}),
                json!({"content": "a"}),
                json!({"reasoning": "y"}),
                json!({"content": "b"}),
            ],
            vec!["reasoning", "text"],
        ),
        (
            json!({"role": "assistant", "reasoning_details": [encrypted], "tool_calls": [first]}),
            vec![
                json!({"role": "assistant", "reasoning_details": [encrypted]}),
                json!({"tool_calls": [indexed(&first, 0)]}),
            ],
            vec!["reasoning", "call"],
        ),
        (
            json!({"role": "assistant", "content": "Checking.", "tool_calls": [first, second],
                "reasoning_details": [signs_call]}),
            vec![
                json!({"role": "assistant", "content": "Checking."}),
                json!({"tool_calls": [indexed(&first, 0)], "reasoning_details": [signs_call]}),
                json!({"tool_calls": [indexed(&second, 1)]}),
            ],
            vec!["reasoning", "text", "call", "call"],
        ),
    ];
    for (message, deltas, kinds) in cases {
        let finish = if message.get("tool_calls").is_some() {
            "tool_calls"
        } else {
            "stop"
        };
        let unary = decode(&openrouter, model, &[whole(message.clone(), finish)]);
        let mut frames: Vec<String> = deltas.into_iter().map(|delta| chunk(delta, None)).collect();
        frames.push(chunk(json!({}), Some(finish)));
        frames.push(DONE.into());
        let streamed = decode(&openrouter, model, &frames);
        assert_eq!(unary.message.content, streamed.message.content, "{message}");
        assert_eq!(super::kinds(&streamed), kinds, "{message}");
        let body = |response| {
            replayed(
                "openrouter",
                model,
                vec![Message::user("q"), turn(response)],
            )
        };
        let replayed = body(&streamed);
        assert_eq!(body(&unary), replayed, "{message}");
        for key in ["reasoning", "reasoning_details"] {
            assert_eq!(replayed.get(key), message.get(key), "{replayed}");
        }
    }
}

/// A late signature goes back with the reasoning it signs, as one message
/// held them.
#[test]
fn a_late_signature_goes_back_with_its_reasoning() {
    let signed =
        json!([{"type": "reasoning.text", "text": "", "signature": "sig-late", "index": 0}]);
    let response = decode(
        &provider("openrouter"),
        "m",
        &[
            chunk(json!({"reasoning": "thinking"}), None),
            chunk(json!({"content": "answer"}), None),
            chunk(json!({"reasoning_details": signed}), None),
            chunk(json!({}), Some("stop")),
            DONE.into(),
        ],
    );
    let sent = replayed("openrouter", "m", vec![Message::user("q"), turn(&response)]);
    assert_eq!(sent["reasoning"], "thinking");
    assert_eq!(sent["reasoning_details"], signed);
    assert_eq!(sent["content"], "answer");
}
