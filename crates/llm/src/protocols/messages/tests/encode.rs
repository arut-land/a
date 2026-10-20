//! Request bodies: rig's `completion` and `wire` tests.

use super::*;
use crate::Meta;
use crate::spec::{Media, Opaque, Output, Reasoning, Source, Text, ToolCall, UserPart};

/// The last message of `history` on the wire to Claude Sonnet 4.6.
fn last(history: Vec<Message>) -> Result<Value, Error> {
    let provider = provider("anthropic");
    let mut request = request(SONNET_4_6, history);
    request.tools = vec![tool("add")].into();
    encoded(&provider, &request).map(|(body, _)| {
        body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .cloned()
            .unwrap_or_default()
    })
}

fn user_media(media: Media) -> Result<Value, Error> {
    last(vec![Message::User(crate::User {
        content: vec![UserPart::Media(media)],
    })])
}

fn media(media_type: &str, source: Source) -> Media {
    Media {
        media_type: media_type.into(),
        source,
        meta: Meta::new(),
    }
}

fn data(data: &str) -> Source {
    Source::Data {
        data: data.to_owned(),
    }
}

/// A native assistant turn of `content` from Claude Sonnet 4.6 on Anthropic.
fn native(content: Vec<AssistantPart>) -> Message {
    Message::Assistant(Assistant {
        content,
        origin: Some(crate::origin(&provider("anthropic"), SONNET_4_6)),
    })
}

/// `content` replayed in a native turn.
fn replayed(content: Vec<AssistantPart>) -> Value {
    let body = sent(
        &provider("anthropic"),
        &request(
            SONNET_4_6,
            vec![Message::user("go"), native(content), Message::user("next")],
        ),
    );
    body["messages"][1]["content"].clone()
}

/// A part holding `item` as its current provider item.
fn with_item(mut part: AssistantPart, item: &Value) -> AssistantPart {
    let canon = match &part {
        AssistantPart::Text(text) => crate::replay::Canon::Text(&text.text),
        AssistantPart::Reasoning(reasoning) => {
            crate::replay::Canon::Reasoning(&reasoning.text, reasoning.redacted)
        }
        AssistantPart::ToolCall(call) => crate::replay::Canon::Call(&call.name, &call.input),
        _ => crate::replay::Canon::Other,
    };
    let fp = crate::replay::fingerprint(canon);
    part.meta_mut()
        .insert(KEY.to_owned(), &json!({"item": item, "fp": fp}));
    part
}

fn call(id: &str, name: &str, input: Value) -> AssistantPart {
    let Value::Object(input) = input else {
        panic!("arguments are an object");
    };
    AssistantPart::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        input,
        invalid: None,
        provider_executed: false,
        meta: Meta::new(),
    })
}

fn thinking(text: &str) -> AssistantPart {
    AssistantPart::Reasoning(Reasoning {
        text: text.to_owned(),
        redacted: false,
        meta: Meta::new(),
    })
}

#[test]
fn opus_4_8_keeps_a_mid_conversation_system_message_in_place() {
    let body = sent(
        &provider("anthropic"),
        &request(
            OPUS_4_8,
            vec![
                Message::system("Top-level instruction."),
                Message::system("Global history instruction."),
                Message::user("Review this code."),
                Message::system("From now on, require explicit type annotations."),
            ],
        ),
    );
    assert_eq!(body["system"][0]["text"], "Top-level instruction.");
    assert_eq!(body["system"][1]["text"], "Global history instruction.");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "system");
    assert_eq!(
        messages[1]["content"][0]["text"],
        "From now on, require explicit type annotations."
    );
}

/// A misplaced mid-conversation instruction moves after the next user turn
/// rather than into `system`; a model that takes none hoists it.
#[test]
fn a_misplaced_system_message_moves_to_the_next_slot() {
    let history = vec![
        Message::system("Global history instruction."),
        Message::assistant("Acknowledged."),
        Message::system("Mid-conversation instruction."),
        Message::user("Answer from the document."),
    ];
    let body = sent(&provider("anthropic"), &request(OPUS_4_8, history.clone()));
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "Global history instruction."}])
    );
    let roles: Vec<&Value> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| &message["role"])
        .collect();
    assert_eq!(roles, ["assistant", "user", "system"]);

    let body = sent(&provider("anthropic"), &request(SONNET_4_6, history));
    assert_eq!(body["system"][1]["text"], "Mid-conversation instruction.");
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(2));
}

/// A code-execution turn ends in its `container` block, which goes to the
/// request's top level: a system message after the turn's last result
/// stays in place rather than joining `system`.
#[test]
fn a_system_message_after_a_code_execution_result_stays_in_place() {
    let provider = provider("anthropic");
    let turn = turn_on(
        &provider,
        OPUS_4_8,
        json!({
            "stop_reason": "end_turn",
            "container": {"id": "container_1", "expires_at": "2026-10-03T00:00:00Z"},
            "content": [
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "code_execution",
                    "input": {"code": "print(1)"}},
                {"type": "code_execution_tool_result", "tool_use_id": "srvtoolu_1",
                    "content": {"type": "code_execution_result", "stdout": "1\n", "stderr": "",
                        "return_code": 0, "content": []}}
            ]
        }),
    );
    let body = sent(
        &provider,
        &request(
            OPUS_4_8,
            vec![
                Message::user("run it"),
                Message::Assistant(turn),
                Message::system("Answer in Spanish."),
            ],
        ),
    );
    assert!(body.get("system").is_none(), "{body:#}");
    assert_eq!(body["messages"][2]["role"], "system", "{body:#}");
    assert_eq!(body["container"], json!("container_1"));
    assert_eq!(
        body["messages"][1]["content"].as_array().map(Vec::len),
        Some(2)
    );
}

/// A system message between a call and its result moves after the result,
/// where Anthropic takes it; a trailing one after an assistant turn (no
/// user turn follows) is hoisted.
#[test]
fn sonnet_5_5_defers_a_system_message_past_tool_results() {
    let provider = provider("anthropic");
    let body = sent(
        &provider,
        &request(
            SONNET_5_5,
            vec![
                Message::user("Look it up."),
                native(vec![call("toolu_1", "add", json!({}))]),
                Message::system("Answer in Spanish."),
                answer("toolu_1", "ok"),
            ],
        ),
    );
    assert!(body.get("system").is_none(), "{body}");
    let roles: Vec<&Value> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| &message["role"])
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "system"]);

    let body = sent(
        &provider,
        &request(
            SONNET_5_5,
            vec![
                Message::user("Look it up."),
                Message::assistant("Done."),
                Message::system("Answer in Spanish."),
            ],
        ),
    );
    assert_eq!(body["system"][0]["text"], "Answer in Spanish.");
}

/// A part whose provider item is current replays the item verbatim:
/// signed thinking, redacted thinking, a call with its `caller`, cited text.
#[test]
fn current_provider_items_replay_verbatim() {
    let signed = json!({"type": "thinking", "thinking": "step one", "signature": "sig-1"});
    let redacted = json!({"type": "redacted_thinking", "data": "redacted block"});
    let use_ = json!({"type": "tool_use", "id": "toolu_1", "name": "add",
        "input": {"x": 1}, "caller": {"type": "direct"}});
    let cited = json!({"type": "text", "text": "Two.", "citations": [{"type": "char_location",
        "cited_text": "2", "document_index": 0, "start_char_index": 0, "end_char_index": 1}]});
    let content = vec![
        with_item(thinking("step one"), &signed),
        with_item(
            AssistantPart::Reasoning(Reasoning {
                redacted: true,
                ..Reasoning::default()
            }),
            &redacted,
        ),
        with_item(call("toolu_1", "add", json!({"x": 1})), &use_),
        with_item(AssistantPart::Text(Text::new("Two.")), &cited),
    ];
    let mut history = vec![Message::user("go"), native(content)];
    history.push(answer("toolu_1", "2"));
    let body = sent_with(
        &provider("anthropic"),
        SONNET_4_6,
        history,
        vec![tool("add")],
    );
    assert_eq!(
        body["messages"][1]["content"],
        json!([signed, redacted, use_, cited])
    );
}

/// pi's rules for parts with no current item: thinking goes as text,
/// redacted thinking (whose payload lived in its item) is dropped, and an
/// edited part is rebuilt from its fields.
#[test]
fn parts_without_a_current_item_rebuild_as_pi_does() {
    let mut edited = with_item(
        AssistantPart::Text(Text::new("before")),
        &json!({"type": "text", "text": "before", "citations": []}),
    );
    if let AssistantPart::Text(text) = &mut edited {
        "after".clone_into(&mut text.text);
    }
    let content = replayed(vec![
        thinking("thought"),
        AssistantPart::Reasoning(Reasoning {
            redacted: true,
            ..Reasoning::default()
        }),
        edited,
    ]);
    assert_eq!(
        content,
        json!([{"type": "text", "text": "thought"}, {"type": "text", "text": "after"}])
    );
}

/// An assistant image goes as its placeholder: assistant turns take none.
#[test]
fn an_assistant_image_is_sent_as_its_placeholder() {
    let content = replayed(vec![AssistantPart::Media(media("image/png", data("aGk=")))]);
    assert_eq!(
        content,
        json!([{"type": "text", "text": super::super::encode::ASSISTANT_IMAGE_OMITTED}])
    );
}

/// A document's fields under this protocol's key (`title`, `citations`)
/// join its block.
#[test]
fn a_document_carries_the_fields_its_state_names() {
    let mut document = media("text/plain", data("aGVsbG8="));
    document.meta.insert(
        KEY.to_owned(),
        &json!({"title": "My Doc", "citations": {"enabled": true}}),
    );
    let message = user_media(document).expect("the document converts");
    let block = &message["content"][0];
    assert_eq!(block["citations"]["enabled"], true);
    assert_eq!(block["title"], "My Doc");
    assert!(block.get("context").is_none());
    assert_eq!(
        block["source"],
        json!({"type": "text", "media_type": "text/plain", "data": "hello"})
    );
}

/// The media the encoder carries: images by typed base64, URL or file id,
/// in a user turn or a tool result; documents by file id, PDF data or URL,
/// or the text they hold. Audio and video never.
#[test]
fn media_goes_in_exactly_the_forms_anthropic_takes() {
    let url = |url: &str| Source::Url {
        url: url.to_owned(),
    };
    let file = |id: &str| Source::File { id: id.into() };
    let images = [
        (media("image/png", data("aGk=")), true),
        (media("image/heic", data("aGk=")), false),
        (media("image/png", url("https://example.com/a.png")), true),
        (media("image/png", file("file_image")), true),
    ];
    for (image, carried) in images {
        assert_eq!(user_media(image.clone()).is_ok(), carried, "{image:?}");
        let result = last(vec![
            Message::user("shoot"),
            native(vec![call("toolu_1", "shot", json!({}))]),
            Message::Tool(crate::ToolMessage {
                results: vec![crate::ToolResult {
                    id: "toolu_1".into(),
                    name: "shot".into(),
                    output: Output::Content(vec![UserPart::Media(image.clone())]),
                    error: false,
                    meta: Meta::new(),
                }],
            }),
        ]);
        assert_eq!(result.is_ok(), carried, "{image:?} in a result");
        if carried {
            let block = &result.expect("carried")["content"][0];
            assert_eq!(block["type"], "tool_result");
            assert_eq!(block["content"][0]["type"], "image");
        }
    }
    let documents = [
        (media("application/pdf", data("JVBERi0=")), true),
        (
            media("application/pdf", url("https://example.com/a.pdf")),
            true,
        ),
        (media("", url("https://example.com/a.pdf")), true),
        (media("text/plain", url("https://example.com/a.txt")), false),
        (media("", file("file_doc")), true),
        (media("text/plain", data("cGxhaW4=")), true),
        (media("text/csv", data("cmlnLG1hdHJpeAo=")), true),
        (media("text/csv", data("//79")), false),
        (media("", data("cGxhaW4=")), false),
        (media("audio/mpeg", data("SUQz")), false),
        (media("video/mp4", url("https://example.com/a.mp4")), false),
    ];
    for (document, carried) in documents {
        assert_eq!(
            user_media(document.clone()).is_ok(),
            carried,
            "{document:?}"
        );
    }
}

/// A text-family document goes as a text source, its base64 data decoded.
#[test]
fn a_text_family_document_is_sent_as_its_text() {
    for (media_type, encoded, text) in [
        ("text/csv", "cmlnLG1hdHJpeAoxLDIK", "rig,matrix\n1,2\n"),
        ("text/plain", "cGxhaW4gdGV4dA==", "plain text"),
        ("text/markdown", "IyBub3Rlcw", "# notes"),
    ] {
        let message = user_media(media(media_type, data(encoded))).expect("the document converts");
        assert_eq!(
            message["content"][0]["source"],
            json!({"type": "text", "media_type": "text/plain", "data": text})
        );
    }
}

/// A same-model turn that ran in a container and called a tool from it:
/// a leading `fallback` marker, which never replays, then the call.
fn container_turn() -> Assistant {
    turn_on(
        &provider("anthropic"),
        SONNET_4_6,
        json!({
            "stop_reason": "tool_use",
            "container": {"id": "container_1", "expires_at": "2026-10-03T00:00:00Z"},
            "content": [
                {"type": "fallback", "model": SONNET_4_6},
                {"type": "tool_use", "id": "toolu_1", "name": "add", "input": {"x": 1},
                    "caller": {"type": "code_execution_20250825", "tool_id": "srvtoolu_1"}}
            ]
        }),
    )
}

fn answered(turn: Assistant) -> Value {
    sent_with(
        &provider("anthropic"),
        SONNET_4_6,
        vec![
            Message::user("add"),
            Message::Assistant(turn),
            answer("toolu_1", "2"),
        ],
        vec![tool("add")],
    )
}

fn edit_calls(turn: &mut Assistant) {
    for part in &mut turn.content {
        if let AssistantPart::ToolCall(call) = part {
            call.input = json!({"x": 5}).as_object().cloned().unwrap_or_default();
        }
    }
}

/// The container is conversation state: a same-model turn names it after
/// its `fallback` block is dropped and after its call is edited, and the
/// edited call keeps the `caller` that ties it to the code execution.
#[test]
fn the_container_and_caller_survive_an_edited_call() {
    let body = answered(container_turn());
    assert_eq!(body["container"], json!("container_1"));
    assert_eq!(
        body["messages"][1]["content"].as_array().map(Vec::len),
        Some(1)
    );

    let mut edited = container_turn();
    edit_calls(&mut edited);
    let body = answered(edited);
    assert_eq!(body["container"], json!("container_1"));
    assert_eq!(
        body["messages"][1]["content"][0],
        json!({"type": "tool_use", "id": "toolu_1", "name": "add", "input": {"x": 5},
            "caller": {"type": "code_execution_20250825", "tool_id": "srvtoolu_1"}})
    );
}

/// A paused turn goes back as it is to resume it, its running
/// `server_tool_use` included.
#[test]
fn a_paused_turn_resumes_with_its_running_server_tool_use() {
    let turn = turn_on(
        &provider("anthropic"),
        SONNET_4_6,
        json!({"stop_reason": "pause_turn", "content": [
            {"type": "text", "text": "Searching."},
            {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                "input": {"query": "rig"}}
        ]}),
    );
    let body = sent_with(
        &provider("anthropic"),
        SONNET_4_6,
        vec![Message::user("search"), Message::Assistant(turn)],
        vec![tool("add")],
    );
    assert_eq!(
        body["messages"][1]["content"][1],
        json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
            "input": {"query": "rig"}})
    );
}

/// A server tool's result replays only with its use.
#[test]
fn a_server_tool_result_never_replays_without_its_use() {
    let mut turn = turn_on(
        &provider("anthropic"),
        SONNET_4_6,
        json!({"stop_reason": "end_turn", "content": [
            {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                "input": {"query": "rig"}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                "content": [{"type": "web_search_result", "url": "https://x", "title": "x",
                    "encrypted_content": "e"}]},
            {"type": "text", "text": "done"}]}),
    );
    assert_eq!(
        replayed(turn.content.clone()).as_array().map(Vec::len),
        Some(3)
    );
    turn.content.remove(0);
    assert_eq!(
        replayed(turn.content),
        json!([{"type": "text", "text": "done"}])
    );
}

/// Text that joins a user turn ahead of its tool results goes after them:
/// Anthropic requires results first.
#[test]
fn tool_results_lead_a_merged_user_message() {
    let body = sent_with(
        &provider("anthropic"),
        SONNET_4_6,
        vec![
            Message::user("add"),
            native(vec![call("toolu_1", "add", json!({"x": 1}))]),
            answer("toolu_1", "2"),
            Message::user("also this"),
        ],
        vec![tool("add")],
    );
    let content = &body["messages"][2]["content"];
    assert_eq!(content[0]["type"], "tool_result", "{body:#}");
    assert_eq!(content[0]["tool_use_id"], "toolu_1");
    assert_eq!(content[1]["text"], "also this");
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(3));
}

/// A model whose thinking binds to its context replays its thinking and
/// asks Anthropic, under the binding beta, to drop a block whose binding no
/// longer matches. A model that does not bind gets neither.
#[test]
fn a_binding_model_asks_for_drop_block() {
    let signed = json!({"type": "thinking", "thinking": "plan", "signature": "sig_opus"});
    let provider = provider("anthropic");
    for (model, binds) in [(OPUS_5_5, true), (SONNET_4_6, false)] {
        let turn = turn_on(
            &provider,
            model,
            json!({"stop_reason": "end_turn", "content": [signed.clone(), {"type": "text", "text": "done"}]}),
        );
        let mut request = request(
            model,
            vec![
                Message::user("go"),
                Message::Assistant(turn),
                Message::user("next"),
            ],
        );
        request.tools = vec![tool("add"), tool("mul")].into();
        let body = sent(&provider, &request);
        assert_eq!(body["messages"][1]["content"][0], signed, "{model}");
        let expected = binds.then(|| {
            json!({"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}})
        });
        assert_eq!(body.get("thinking"), expected.as_ref(), "{model}");
        assert_eq!(
            beta(&provider, &request).as_deref(),
            binds.then_some(THINKING_BINDING_BETA),
            "{model}"
        );
    }
}

/// The caller's thinking stays, with `drop_block` added; thinking the
/// caller turned to another mode gets none.
#[test]
fn drop_block_joins_the_callers_thinking() {
    for (thinking, sent_as) in [
        (
            json!({"type": "adaptive", "display": "summarized"}),
            json!({"type": "adaptive", "display": "summarized",
                "block_binding": {"prefix_mismatch_behavior": "drop_block"}}),
        ),
        (json!({"type": "disabled"}), json!({"type": "disabled"})),
        (
            json!({"type": "between_tools"}),
            json!({"type": "between_tools"}),
        ),
    ] {
        let mut request = request(OPUS_5_5, vec![Message::user("q")]);
        request
            .options
            .insert(KEY.to_owned(), json!({"thinking": thinking}));
        assert_eq!(sent(&provider("anthropic"), &request)["thinking"], sent_as);
        let binding = beta(&provider("anthropic"), &request);
        assert_eq!(binding.is_some(), sent_as.get("block_binding").is_some());
    }
}

#[test]
fn context_binding_reads_every_spelling_of_a_claude_model() {
    for model in [
        "claude-opus-5-5",
        "claude-opus-5-5-20260101",
        "claude-opus-5",
        "claude-fable-5-1",
        "claude-sonnet-5-5",
        "anthropic/claude-opus-5.5",
        "claude-opus-5.5",
        "anthropic.claude-opus-5-5-v1:0",
        "us.anthropic.claude-opus-5-5-20260101-v1:0",
        "us.anthropic.claude-opus-5",
    ] {
        assert!(crate::models::anthropic::binds_context(model), "{model}");
    }
    for model in [
        "claude-sonnet-5",
        "claude-haiku-4-5-20251001",
        "us.anthropic.claude-haiku-4-5-20251001-v1:0",
        "gpt-5",
    ] {
        assert!(!crate::models::anthropic::binds_context(model), "{model}");
    }
}

/// What Anthropic would reject in `body`: an empty message or blank text,
/// a system message outside a slot, and a call its next message does not
/// answer first.
fn violations(body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    for (i, message) in messages.iter().enumerate() {
        let content = message["content"].as_array().cloned().unwrap_or_default();
        if content.is_empty() {
            out.push(format!("message {i} has empty content"));
        }
        for block in &content {
            if block["type"] == "text"
                && block["text"].as_str().is_some_and(|t| t.trim().is_empty())
            {
                out.push(format!("message {i} has blank text"));
            }
        }
        if message["role"] == "system" {
            let prev = messages[..i]
                .iter()
                .rev()
                .find(|m| m["role"] != "system")
                .map(|m| m["role"].clone());
            let next = messages[i + 1..]
                .iter()
                .find(|m| m["role"] != "system")
                .map(|m| m["role"].clone());
            if prev != Some(json!("user")) || !(next.is_none() || next == Some(json!("assistant")))
            {
                out.push(format!("system at {i} between {prev:?} and {next:?}"));
            }
        }
        if message["role"] == "assistant" {
            let ids: Vec<&str> = content
                .iter()
                .filter(|block| block["type"] == "tool_use")
                .filter_map(|block| block["id"].as_str())
                .collect();
            if ids.is_empty() {
                continue;
            }
            let Some(next) = messages.get(i + 1) else {
                out.push(format!("tool_use at {i} ends the request"));
                continue;
            };
            let leading: Vec<&str> = next["content"]
                .as_array()
                .into_iter()
                .flatten()
                .take_while(|block| block["type"] == "tool_result")
                .filter_map(|block| block["tool_use_id"].as_str())
                .collect();
            if next["role"] != "user" || ids.iter().any(|id| !leading.contains(id)) {
                out.push(format!(
                    "tool_use at {i} not answered first by next message"
                ));
            }
        }
    }
    out
}

/// Histories `prepare` and the encoder must shape into a request Anthropic
/// takes, on a model that keeps system messages in place and on one that
/// folds them into the prompt.
#[test]
fn adversarial_histories_encode_to_requests_anthropic_takes() {
    let other = Some(crate::Origin {
        protocol: "openai.chat".into(),
        provider: "openai".into(),
        model: "gpt-4.1".into(),
    });
    let foreign = |content: Vec<AssistantPart>| {
        Message::Assistant(Assistant {
            content,
            origin: other.clone(),
        })
    };
    let text = |text: &str| AssistantPart::Text(Text::new(text));
    let calls = || call("x", "lookup", json!({}));
    let cases: Vec<(&str, Vec<Message>)> = vec![
        (
            "system between assistants",
            vec![
                Message::user("a"),
                foreign(vec![text("one")]),
                Message::system("mid"),
                foreign(vec![text("two")]),
                Message::user("q"),
            ],
        ),
        (
            "system while a call waits, then text",
            vec![
                Message::user("a"),
                foreign(vec![calls()]),
                Message::system("mid"),
                Message::user("note"),
                answer("x", "r"),
                Message::user("q"),
            ],
        ),
        (
            "blank user after a call",
            vec![
                Message::user("a"),
                foreign(vec![calls()]),
                Message::user("  "),
                answer("x", "r"),
            ],
        ),
        (
            "two systems around a user",
            vec![
                Message::system("s0"),
                Message::user("a"),
                Message::system("s1"),
                Message::user("b"),
                Message::system("s2"),
                foreign(vec![text("ok")]),
                Message::user("end"),
            ],
        ),
        (
            "system before results of an aborted turn",
            vec![
                Message::user("a"),
                foreign(vec![calls()]),
                Message::system("mid"),
                answer("x", "r"),
                Message::user("q"),
            ],
        ),
    ];
    let provider = provider("anthropic");
    let mut failures = Vec::new();
    for model in [OPUS_5_5, SONNET_4_6] {
        for (label, history) in &cases {
            let body = sent_with(&provider, model, history.clone(), vec![tool("lookup")]);
            let found = violations(&body);
            if !found.is_empty() {
                failures.push(format!(
                    "{model} {label}: {found:?}\n  {}",
                    body["messages"]
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Opaque parts of another origin never reach Anthropic, nor do those
/// that do not replay.
#[test]
fn an_opaque_part_replays_only_where_it_may() {
    let mut opaque = AssistantPart::Opaque(Opaque::default());
    opaque.meta_mut().insert(
        KEY.to_owned(),
        &json!({"item": {"type": "frobnicate"}, "replay": false}),
    );
    let content = replayed(vec![opaque, AssistantPart::Text(Text::new("hi"))]);
    assert_eq!(content, json!([{"type": "text", "text": "hi"}]));
}
