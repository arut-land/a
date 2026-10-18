//! Ported from rig's `streaming/tests.rs` and the decoding half of
//! `completion/tests.rs`.

use super::*;

#[test]
fn a_blocked_prompt_names_its_reason_and_ratings() {
    let error = decoded(&[json!({
        "promptFeedback": {
            "blockReason": "PROHIBITED_CONTENT",
            "safetyRatings": [{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "probability": "HIGH"}]
        },
        "usageMetadata": {"promptTokenCount": 12, "totalTokenCount": 12}
    })])
    .expect_err("blocked");
    assert_eq!(error.kind, Kind::ContentFilter);
    assert!(!error.is_retryable());
    for needle in [
        "blocked the prompt",
        "PROHIBITED_CONTENT",
        "HARM_CATEGORY_DANGEROUS_CONTENT=HIGH",
    ] {
        assert!(error.message.contains(needle), "{}", error.message);
    }
}

/// `SAFETY`, `BLOCKLIST` and `PROHIBITED_CONTENT` judge the content and are
/// final. `OTHER` is Google's "blocked for unknown reasons", answered on the
/// next call, and a reason no rule knows is no verdict either.
#[test]
fn block_reasons_split_into_refusals_and_transient_blocks() {
    for (reason, retryable) in [
        ("SAFETY", false),
        ("BLOCKLIST", false),
        ("PROHIBITED_CONTENT", false),
        ("OTHER", true),
        ("SOMETHING_NEW", true),
    ] {
        let error =
            decoded(&[json!({"promptFeedback": {"blockReason": reason}})]).expect_err(reason);
        assert_eq!(error.is_retryable(), retryable, "{reason}: {error:?}");
        assert!(
            error.message.contains(&format!("block_reason={reason}")),
            "{reason}: {}",
            error.message
        );
    }
    let error = decoded(&[json!({"promptFeedback": {
        "blockReason": "OTHER",
        "safetyRatings": [{"category": "HARM_CATEGORY_JAILBREAK", "probability": "SOMEDAY"}]
    }})])
    .expect_err("blocked");
    assert!(
        error.message.contains("HARM_CATEGORY_JAILBREAK=SOMEDAY"),
        "{error}"
    );
}

/// A block ends the turn: the decoder fails at it, so nothing after it is
/// read.
#[test]
fn a_block_without_usage_still_ends_the_turn() {
    let error = decoded(&[
        json!({"promptFeedback": {"blockReason": "SAFETY"}}),
        chunk(json!([{"text": "dead"}]), Some("STOP")),
    ])
    .expect_err("blocked");
    assert_eq!(error.kind, Kind::ContentFilter);
}

/// The zero value names no block, and a reply with no candidate never
/// ended.
#[test]
fn an_unspecified_block_reason_is_no_block() {
    let error = decoded(&[json!({"promptFeedback": {"blockReason": "BLOCK_REASON_UNSPECIFIED"}})])
        .expect_err("no candidates");
    assert_eq!(error.kind, Kind::Incomplete);
}

#[test]
fn an_unknown_finish_reason_keeps_its_spelling() {
    let response = decoded(&[chunk(
        json!([{"text": "done"}]),
        Some("FINISH_REASON_FUTURE"),
    )])
    .expect("the reply decodes");
    assert_eq!(response.message.text(), "done");
    assert_eq!(response.finish.reason, Reason::Other);
    assert_eq!(response.finish.raw.as_deref(), Some("FINISH_REASON_FUTURE"));
    let response = decoded(&[json!({"candidates": [{"finishReason": 9}]})]).expect("decodes");
    assert_eq!(response.finish.raw.as_deref(), Some("FINISH_REASON_9"));
}

#[test]
fn the_reply_names_its_model_id_and_length_stop() {
    let response = decoded(&[json!({
        "responseId": "resp-meta",
        "modelVersion": "gemini-2.0-flash-001",
        "candidates": [{"content": {"parts": [{"text": "hi"}], "role": "model"}, "finishReason": "MAX_TOKENS"}]
    })])
    .expect("the reply decodes");
    assert_eq!(response.model.as_deref(), Some("gemini-2.0-flash-001"));
    assert_eq!(response.id.as_deref(), Some("resp-meta"));
    assert_eq!(response.finish.reason, Reason::Length);
}

/// A failure is final: it ends the reply at once, with what Gemini said.
#[test]
fn a_failure_reason_ends_the_reply_with_its_message() {
    let response = decoded(&[
        json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL", "finishMessage": "Malformed function call: x"}]}),
        chunk(json!([{"text": "never read"}]), Some("STOP")),
    ])
    .expect("the reply decodes");
    assert_eq!(response.finish.reason, Reason::Other);
    assert_eq!(
        response.finish.raw.as_deref(),
        Some("MALFORMED_FUNCTION_CALL")
    );
    assert_eq!(
        response.finish.message.as_deref(),
        Some("Malformed function call: x")
    );
    assert_eq!(response.message.content, []);
    for (name, reason) in [
        ("SAFETY", Reason::ContentFilter),
        ("RECITATION", Reason::Other),
        ("SPII", Reason::ContentFilter),
    ] {
        let response = decoded(&[chunk(json!([{"text": "a"}]), Some(name))]).expect(name);
        assert_eq!(response.finish.reason, reason, "{name}");
        assert_eq!(response.message.text(), "a", "{name}");
    }
}

/// A stop waits for the end of the body: a hosted-tool round can report one
/// before more content arrives. The usage message beside it is no
/// explanation.
#[test]
fn a_stop_waits_for_the_end_of_the_body() {
    let response = decoded(&[
        json!({"candidates": [{"content": {"parts": [{"text": "a"}]}, "finishReason": "STOP", "finishMessage": "Model generated function call(s)."}]}),
        chunk(json!([{"text": "b"}]), Some("STOP")),
    ])
    .expect("the reply decodes");
    assert_eq!(response.message.text(), "ab");
    assert_eq!(response.finish.message, None);
}

#[test]
fn a_body_that_ends_without_a_finish_reason_is_incomplete() {
    let error = decoded(&[chunk(json!([{"text": "cut"}]), None)]).expect_err("incomplete");
    assert_eq!(error.kind, Kind::Incomplete);
}

#[test]
fn usage_adds_tool_prompts_to_input_and_thoughts_to_output() {
    let response = decoded(&[json!({
        "candidates": [{"content": {"parts": [{"text": "kept"}]}, "finishReason": "STOP"}],
        "usageMetadata": {
            "promptTokenCount": 23, "toolUsePromptTokenCount": 620, "candidatesTokenCount": 277,
            "thoughtsTokenCount": 315, "cachedContentTokenCount": 7, "totalTokenCount": 1235,
            "trafficType": "ON_DEMAND_PRIORITY", "promptTokensDetails": [{"modality": "HOLOGRAM", "tokenCount": 3}]
        }
    })])
    .expect("a usage label never fails the reply");
    let usage = response.usage;
    assert_eq!(usage.input, Some(643));
    assert_eq!(usage.output, Some(592));
    assert_eq!(usage.reasoning, Some(315));
    assert_eq!(usage.cache_read, Some(7));
}

/// A usage-only chunk opens no part and ends nothing; the finish after it
/// reports the identity it carried.
#[test]
fn a_usage_only_chunk_carries_identity_and_counts() {
    let response = decoded(&[
        json!({
            "responseId": "response-123",
            "modelVersion": "gemini-2.0-flash-001",
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}
        }),
        json!({"candidates": [{"content": {"parts": [{"text": "ok"}], "role": "model"}, "finishReason": "STOP"}]}),
    ])
    .expect("the reply decodes");
    assert_eq!(response.id.as_deref(), Some("response-123"));
    assert_eq!(response.model.as_deref(), Some("gemini-2.0-flash-001"));
    assert_eq!(response.message.text(), "ok");
    assert_eq!(response.usage.input, Some(10));
    assert_eq!(response.usage.output, Some(5));
}

/// Consecutive text parts continue one part, whose item holds the merged
/// text.
#[test]
fn consecutive_text_parts_are_one_part() {
    let response = decoded(&[chunk(
        json!([{"text": "Hello, "}, {"text": "world!"}, {"text": " How are you?"}]),
        Some("STOP"),
    )])
    .expect("the reply decodes");
    let [part @ AssistantPart::Text(text)] = response.message.content.as_slice() else {
        panic!("one text: {:?}", response.message.content);
    };
    assert_eq!(text.text, "Hello, world! How are you?");
    assert_eq!(
        item(part),
        Some(&json!({"text": "Hello, world! How are you?"}))
    );
}

#[test]
fn every_call_is_a_part_with_its_id() {
    let response = decoded(&[chunk(
        json!([
            {"functionCall": {"name": "get_weather", "args": {"city": "San Francisco"}, "id": "call-weather"}},
            {"functionCall": {"name": "get_temperature", "args": {"location": "New York"}, "id": "call-temperature"}}
        ]),
        Some("STOP"),
    )])
    .expect("the reply decodes");
    let calls: Vec<_> = response
        .message
        .tool_calls()
        .map(|call| {
            (
                call.id.as_str(),
                call.name.as_str(),
                Value::Object(call.input.clone()),
            )
        })
        .collect();
    assert_eq!(
        calls,
        [
            (
                "call-weather",
                "get_weather",
                json!({"city": "San Francisco"})
            ),
            (
                "call-temperature",
                "get_temperature",
                json!({"location": "New York"})
            ),
        ]
    );
    assert_eq!(response.finish.reason, Reason::ToolCalls);
}

#[test]
fn mixed_parts_keep_their_order() {
    let response = decoded(&[json!({
        "candidates": [{"content": {"parts": [
            {"text": "Let me think about this...", "thought": true},
            {"text": "Here's my response: "},
            {"functionCall": {"name": "search", "args": {"query": "rust async"}}},
            {"text": "I found the answer!"}
        ]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 50, "thoughtsTokenCount": 15}
    })])
    .expect("the reply decodes");
    let [
        AssistantPart::Reasoning(thought),
        AssistantPart::Text(text),
        AssistantPart::ToolCall(call),
        AssistantPart::Text(last),
    ] = response.message.content.as_slice()
    else {
        panic!(
            "a thought, text, a call and text: {:?}",
            response.message.content
        );
    };
    assert_eq!(thought.text, "Let me think about this...");
    assert_eq!(text.text, "Here's my response: ");
    assert_eq!(call.name, "search");
    // Gemini 2.5 sends no call id: one is issued.
    assert!(call.id.starts_with("call_"), "{}", call.id);
    assert_eq!(last.text, "I found the answer!");
    assert_eq!(response.usage.reasoning, Some(15));
}

#[test]
fn an_empty_reply_is_a_stop_with_no_parts() {
    let response = decoded(&[chunk(json!([]), Some("STOP"))]).expect("an empty reply decodes");
    assert_eq!(response.message.content, []);
    assert_eq!(response.finish.reason, Reason::Stop);
}

#[test]
fn wrongly_typed_candidates_fail_the_reply() {
    for chunk in [
        json!({"candidates": {"content": {}}}),
        json!({"candidates": ["stray"]}),
        json!({"candidates": [{"content": "stray"}]}),
        json!({"candidates": [{"content": {"parts": {}}}]}),
    ] {
        let error = decoded(std::slice::from_ref(&chunk)).expect_err("malformed");
        assert_eq!(error.kind, Kind::Decode, "{chunk}");
    }
    let mut decoder = super::super::decode::GeminiDecoder::new();
    let error = Decoder::frame(
        &mut decoder,
        Frame {
            event: "",
            data: "{not json",
        },
        &mut Vec::new(),
    )
    .expect_err("not JSON");
    assert_eq!(error.kind, Kind::Decode);
}

/// An in-band error whose code is an HTTP status classifies as that status
/// would on the reply itself.
#[test]
fn in_band_http_errors_classify_as_their_status() {
    for code in [400_u16, 401, 404, 429, 500, 503] {
        let body = json!({"error": {"code": code, "message": "Keep CASE", "status": "VERDICT"}});
        let error = decoded(std::slice::from_ref(&body)).expect_err("an in-band error");
        let unary = Error::from_status(code, &http::HeaderMap::new(), body.to_string().as_bytes());
        assert_eq!(error.kind, unary.kind, "{code}");
        assert_eq!(error.status, Some(code));
        assert_eq!(error.message, "Keep CASE");
        assert_eq!(error.body.as_deref(), Some(body.to_string().as_str()));
    }
}

#[test]
fn in_band_codes_that_are_no_status_invent_none() {
    for code in [
        json!(null),
        json!("503"),
        json!(14),
        json!(-1),
        json!(200),
        json!(700),
        json!(65536),
    ] {
        let body = json!({"error": {"code": code, "message": "unknown"}});
        let error = decoded(&[body]).expect_err("an in-band error");
        assert_eq!(error.status, None, "{code}");
        assert_eq!(error.message, "unknown");
    }
    let error =
        decoded(&[json!({"error": {"code": 14, "message": "busy", "status": "UNAVAILABLE"}})])
            .expect_err("an in-band error");
    assert_eq!(error.kind, Kind::Server);
}

/// Two thought parts with their own signatures stay two parts: merged, the
/// first signature would be lost.
#[test]
fn two_signed_thoughts_keep_both_signatures() {
    let response = decoded(&[chunk(
        json!([
            {"thought": true, "text": "one", "thoughtSignature": "b25l"},
            {"thought": true, "text": "two", "thoughtSignature": "dHdv"},
            {"text": "answer"}
        ]),
        Some("STOP"),
    )])
    .expect("the reply decodes");
    let signatures: Vec<_> = response
        .message
        .content
        .iter()
        .filter_map(item)
        .filter_map(|item| item.str("thoughtSignature"))
        .collect();
    assert_eq!(signatures, ["b25l", "dHdv"]);
}
