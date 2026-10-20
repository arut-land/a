//! Replies: rig's `streaming` tests and the reply cells of its
//! `completion` tests.

use super::*;
use crate::{Kind, Reason};

/// Signature fragments concatenate onto the opening signature, and the
/// thinking item holds the whole block.
#[test]
fn thinking_assembles_its_text_and_signature_into_its_item() {
    let frames = reply(
        vec![block(
            0,
            json!({"type": "thinking", "thinking": "", "signature": "open-"}),
            &[
                json!({"type": "thinking_delta", "thinking": "Let me "}),
                json!({"type": "thinking_delta", "thinking": "think."}),
                json!({"type": "signature_delta", "signature": "sig_"}),
                json!({"type": "signature_delta", "signature": "end"}),
            ],
        )],
        "end_turn",
    );
    let response = streamed(&frames).expect("the reply decodes");
    let [AssistantPart::Reasoning(reasoning)] = response.message.content.as_slice() else {
        panic!("one reasoning part: {:?}", response.message.content);
    };
    assert_eq!(reasoning.text, "Let me think.");
    assert!(!reasoning.redacted);
    assert_eq!(
        item(&response.message.content[0]).as_ref(),
        Some(
            &json!({"type": "thinking", "thinking": "Let me think.", "signature": "open-sig_end"})
        )
    );
}

#[test]
fn redacted_thinking_is_a_redacted_part_holding_its_payload() {
    let frames = reply(
        vec![block(
            0,
            json!({"type": "redacted_thinking", "data": "redacted_blob"}),
            &[],
        )],
        "end_turn",
    );
    let response = streamed(&frames).expect("the reply decodes");
    let [AssistantPart::Reasoning(reasoning)] = response.message.content.as_slice() else {
        panic!("one reasoning part: {:?}", response.message.content);
    };
    assert!(reasoning.text.is_empty() && reasoning.redacted);
    assert_eq!(
        item(&response.message.content[0]).as_ref(),
        Some(&json!({"type": "redacted_thinking", "data": "redacted_blob"}))
    );
}

/// A call's item holds the input its fragments assembled, and keeps every
/// field the provider sent, `caller` included.
#[test]
fn a_call_assembles_its_input_into_its_item() {
    let frames = reply(
        vec![block(
            0,
            json!({"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {},
                "caller": {"type": "direct"}}),
            &[
                json!({"type": "input_json_delta", "partial_json": "{\"location\":"}),
                json!({"type": "input_json_delta", "partial_json": "\"Paris\"}"}),
            ],
        )],
        "tool_use",
    );
    let response = streamed(&frames).expect("the reply decodes");
    let [AssistantPart::ToolCall(call)] = response.message.content.as_slice() else {
        panic!("one call: {:?}", response.message.content);
    };
    assert_eq!(call.id, "toolu_1");
    assert_eq!(
        call.input.parse::<Value>().unwrap(),
        json!({"location": "Paris"})
    );
    assert_eq!(
        item(&response.message.content[0]).as_ref(),
        Some(
            &json!({"type": "tool_use", "id": "toolu_1", "name": "lookup",
            "input": {"location": "Paris"}, "caller": {"type": "direct"}})
        )
    );
    assert_eq!(response.finish.reason, Reason::ToolCalls);
}

/// A stream that never states its stop reason is incomplete, whatever else
/// it ended with: a `message_delta` without one, or `message_stop` alone.
#[test]
fn a_stream_without_its_stop_reason_never_ends_successfully() {
    let mut frames = reply(
        vec![block(0, json!({"type": "text", "text": "partial"}), &[])],
        "end_turn",
    );
    if let Some(delta) = frames.last_mut() {
        delta["delta"]["stop_reason"] = Value::Null;
    }
    frames.push(json!({"type": "message_stop"}));
    let error = streamed(&frames).expect_err("incomplete");
    assert_eq!(error.kind, Kind::Incomplete);
}

#[test]
fn input_to_a_text_block_fails_the_reply() {
    let frames = reply(
        vec![block(
            0,
            json!({"type": "text", "text": ""}),
            &[json!({"type": "input_json_delta", "partial_json": "{}"})],
        )],
        "end_turn",
    );
    assert_eq!(streamed(&frames).expect_err("fails").kind, Kind::Decode);
}

/// Citations are not modeled, but their deltas land on the text block's
/// item in order, after an opening `null` or empty list alike.
#[test]
fn citation_deltas_land_on_the_text_item() {
    let citation = json!({"type": "char_location", "cited_text": "The grass is green.",
        "document_index": 0, "document_title": "Example", "start_char_index": 0,
        "end_char_index": 20});
    for opening in [json!(null), json!([])] {
        let frames = reply(
            vec![block(
                0,
                json!({"type": "text", "text": "", "citations": opening}),
                &[
                    json!({"type": "citations_delta", "citation": citation}),
                    json!({"type": "text_delta", "text": "the grass is green"}),
                ],
            )],
            "end_turn",
        );
        let response = streamed(&frames).expect("the reply decodes");
        let [AssistantPart::Text(text)] = response.message.content.as_slice() else {
            panic!("one text part: {:?}", response.message.content);
        };
        assert_eq!(text.text, "the grass is green");
        assert_eq!(
            item(&response.message.content[0])
                .as_ref()
                .and_then(|item| item.get("citations")),
            Some(&json!([citation]))
        );
    }
}

/// pi's fallback rule: a leading `fallback` block marks the model that
/// took over and never replays; one after output began fails the reply.
#[test]
fn a_fallback_block_is_kept_first_and_an_error_after_output() {
    let fallback = json!({"type": "fallback", "model": "claude-opus-4-8"});
    let text = json!({"type": "text", "text": ""});
    let leading = streamed(&reply(
        vec![
            block(0, fallback.clone(), &[]),
            block(
                1,
                text.clone(),
                &[json!({"type": "text_delta", "text": "hi"})],
            ),
        ],
        "end_turn",
    ))
    .expect("a leading fallback is legal");
    let first = leading.message.content.first().expect("a part");
    assert!(matches!(first, AssistantPart::Opaque(_)) && !replays(first));
    let late = streamed(&reply(
        vec![
            block(0, text, &[json!({"type": "text_delta", "text": "hi"})]),
            block(1, fallback, &[]),
        ],
        "end_turn",
    ));
    assert!(late.is_err(), "{late:?}");
}

/// An unmodeled top-level event and a `ping` are skipped; a frame that is
/// no JSON fails the reply.
#[test]
fn unknown_events_are_skipped_and_corrupt_frames_fail() {
    let mut frames = reply(
        vec![block(
            0,
            json!({"type": "text", "text": ""}),
            &[json!({"type": "text_delta", "text": "hi"})],
        )],
        "end_turn",
    );
    frames.insert(
        1,
        json!({"type": "something_new_from_anthropic", "field": "x"}),
    );
    frames.insert(1, json!({"type": "ping"}));
    let response = streamed(&frames).expect("the reply decodes");
    assert_eq!(response.message.text(), "hi");

    let provider = provider("anthropic");
    let mut decoder = provider
        .protocol
        .decoder(&target(&provider, SONNET_4_6), &Request::default());
    let error = decoder
        .frame(
            Frame {
                event: "",
                data: "{not json",
            },
            &mut Vec::new(),
        )
        .expect_err("corrupt");
    assert_eq!(error.kind, Kind::Decode);
}

/// A terminal `message_delta` carrying only the output count keeps
/// `message_start`'s cache counters, so the input still counts the cached
/// prefix: 10 uncached, 6 read and 4 written.
#[test]
fn cache_usage_from_message_start_survives_an_output_only_terminal_delta() {
    let frames = [
        json!({"type": "message_start", "message": {"id": "msg_1", "role": "assistant",
            "content": [], "model": SONNET_4_6, "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 10, "output_tokens": 0, "cache_creation_input_tokens": 4,
                "cache_read_input_tokens": 6,
                "cache_creation": {"ephemeral_1h_input_tokens": 3, "ephemeral_5m_input_tokens": 1}}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"output_tokens": 3}}),
    ];
    let usage = streamed(&frames)
        .expect("the message_delta ends the reply")
        .usage;
    assert_eq!(usage.input, Some(10 + 6 + 4));
    assert_eq!(usage.cache_write, Some(4));
    assert_eq!(usage.cache_read, Some(6));
    assert_eq!(usage.output, Some(3));
}

/// A delta that omits its `type` is malformed, not novel: skipping it would
/// turn a gateway's untagged text into an empty reply. So is a known delta
/// whose text is not text.
#[test]
fn a_delta_without_its_type_or_text_fails_the_reply() {
    for delta in [
        json!({"text": "hello"}),
        json!({"type": "text_delta", "text": 42}),
    ] {
        let frames = reply(
            vec![block(
                0,
                json!({"type": "text", "text": ""}),
                std::slice::from_ref(&delta),
            )],
            "end_turn",
        );
        assert_eq!(
            streamed(&frames).expect_err("fails").kind,
            Kind::Decode,
            "{delta}"
        );
    }
}

/// A Bedrock-compatible gateway's `message_start` without a message says
/// nothing.
#[test]
fn a_message_start_with_a_null_message_is_a_no_op() {
    let mut frames = reply(Vec::new(), "end_turn");
    frames[0] = json!({"type": "message_start", "message": null});
    let response = streamed(&frames).expect("the reply decodes");
    assert_eq!(response.message.content, Vec::new());
    assert_eq!(response.id, None);
}

/// The input count prefers the terminal `message_delta`, and falls back to
/// `message_start`'s when the delta omits it or reports a placeholder zero.
#[test]
fn input_tokens_prefer_the_terminal_delta_and_fall_back_to_message_start() {
    let frames = |start: u64, delta: Option<u64>| {
        let mut usage = json!({"output_tokens": 3});
        if let Some(delta) = delta {
            usage["input_tokens"] = json!(delta);
        }
        vec![
            json!({"type": "message_start", "message": {"id": "msg_1", "role": "assistant",
                "content": [], "model": SONNET_4_6, "usage": {"input_tokens": start, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": usage}),
        ]
    };
    for (start, delta, expected) in [
        (0, Some(9), 9),
        (5, None, 5),
        (5, Some(5), 5),
        (5, Some(0), 5),
    ] {
        let usage = streamed(&frames(start, delta))
            .expect("the turn completes")
            .usage;
        assert_eq!(usage.input, Some(expected), "{start} then {delta:?}");
    }
}

/// An `error` event fails the reply with the provider's own type and
/// message.
#[test]
fn an_error_event_fails_the_reply_as_the_provider_classifies_it() {
    let mut frames = reply(Vec::new(), "end_turn");
    frames.insert(
        1,
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    );
    let error = streamed(&frames).expect_err("fails");
    assert_eq!(error.kind, Kind::Server);
    assert_eq!(error.message, "Overloaded");
    assert!(error.is_retryable());
}

/// An item type never seen before, and a field never seen on a known item,
/// survive decoding whole and streamed. The unknown item goes back verbatim
/// to the model that produced it; the known item goes back with its input
/// fields only.
#[test]
fn an_invented_item_and_field_survive_decode_and_same_model_replay() {
    let invented = json!({"type": "frobnicate", "payload": {"x": 1}});
    let text = json!({"type": "text", "text": "hi", "sparkle": true});
    let unary = whole(json!({
        "content": [invented, text], "stop_reason": "end_turn", "stop_sequence": null,
    }))
    .expect("the whole reply decodes");
    let stream = streamed(&reply(
        vec![
            block(0, invented.clone(), &[]),
            block(
                1,
                json!({"type": "text", "text": "", "sparkle": true}),
                &[json!({"type": "text_delta", "text": "hi"})],
            ),
        ],
        "end_turn",
    ))
    .expect("the stream decodes");
    assert_eq!(unary.message, stream.message);
    let provider = provider("anthropic");
    for response in [unary, stream] {
        let body = sent_with(
            &provider,
            SONNET_4_6,
            vec![
                Message::user("hello"),
                Message::Assistant(response.message),
                Message::user("again"),
            ],
            Vec::new(),
        );
        // Input blocks are validated strictly: an invented block goes back
        // whole, an invented field on a known block does not.
        assert_eq!(
            body["messages"][1]["content"],
            json!([invented, {"type": "text", "text": "hi"}])
        );
    }
}

/// A reply with no content decodes the same whole or streamed, whatever its
/// stop reason: the stop reason decides how the turn ends.
#[test]
fn an_empty_reply_decodes_the_same_whole_or_streamed() {
    for (reason, expected) in [
        ("end_turn", Reason::Stop),
        ("stop_sequence", Reason::Stop),
        ("max_tokens", Reason::Length),
        ("model_context_window_exceeded", Reason::Length),
        ("tool_use", Reason::ToolCalls),
        ("pause_turn", Reason::Stop),
        ("refusal", Reason::ContentFilter),
        ("sensitive", Reason::Other),
    ] {
        let streamed = streamed(&reply(vec![], reason)).expect("the stream decodes");
        let whole = whole(json!({"content": [], "stop_reason": reason, "stop_sequence": null}))
            .expect("the message decodes");
        assert_eq!(streamed.finish, whole.finish, "{reason}");
        assert_eq!(streamed.finish.reason, expected, "{reason}");
        assert_eq!(streamed.finish.raw.as_deref(), Some(reason));
    }
}

/// A refusal ends the turn as filtered, with its explanation or a default.
#[test]
fn a_refusal_carries_its_explanation() {
    let mut frames = reply(Vec::new(), "refusal");
    if let Some(delta) = frames.last_mut() {
        delta["delta"]["stop_details"] = json!({"type": "refusal", "category": "cyber",
            "explanation": "Not with that."});
    }
    let finish = streamed(&frames).expect("the reply decodes").finish;
    assert_eq!(finish.reason, Reason::ContentFilter);
    assert_eq!(finish.message.as_deref(), Some("Not with that."));
    let finish = streamed(&reply(Vec::new(), "refusal"))
        .expect("the reply decodes")
        .finish;
    assert_eq!(
        finish.message.as_deref(),
        Some("The model refused to complete the request")
    );
}

/// A stop reason states every block still open complete: thinking whose
/// stop never came keeps its signed item, and so does a call whose input
/// is whole. A call whose input is still cut stays open, and the end of the
/// reply closes it unfinished.
#[test]
fn a_stop_reason_states_every_open_block_complete() {
    let without_stops = |reason: &str, input: &str| {
        let mut frames = reply(Vec::new(), reason);
        let end = frames.pop().expect("the message_delta");
        frames.extend([
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "plan"}}),
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_start", "index": 1,
                "content_block": {"type": "tool_use", "id": "toolu_a", "name": "f", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": input}}),
            end,
        ]);
        frames
    };
    let response = streamed(&without_stops("tool_use", "{\"x\":1}")).expect("the reply decodes");
    assert_eq!(response.finish.reason, Reason::ToolCalls);
    let content = &response.message.content;
    assert_eq!(
        item(&content[0]).as_ref(),
        Some(&json!({"type": "thinking", "thinking": "plan", "signature": "sig"}))
    );
    assert_eq!(
        item(&content[1]).as_ref(),
        Some(&json!({"type": "tool_use", "id": "toolu_a", "name": "f", "input": {"x": 1}}))
    );

    let cut = streamed(&without_stops("max_tokens", "{\"x\":")).expect("the reply decodes");
    assert_eq!(cut.finish.reason, Reason::Length);
    assert!(item(&cut.message.content[0]).is_some(), "{:?}", cut.message);
    assert!(item(&cut.message.content[1]).is_none(), "{:?}", cut.message);
    let unfinished = streamed(&without_stops("tool_use", "{\"x\":"));
    assert_eq!(unfinished.expect_err("unfinished").kind, Kind::Incomplete);
}

/// A gateway that skips `content_block_start` still streams the block: its
/// first delta opens it.
#[test]
fn a_delta_for_a_block_that_never_started_opens_it() {
    let frames = reply(
        vec![vec![
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "hi"}}),
            json!({"type": "content_block_stop", "index": 0}),
        ]],
        "end_turn",
    );
    let response = streamed(&frames).expect("the reply decodes");
    assert!(
        matches!(response.message.content.as_slice(), [AssistantPart::Text(text)] if text.text == "hi"),
        "{:?}",
        response.message
    );
}

/// A `tool_use` whose `input` is not an object keeps its call, its input
/// text the invalid arguments, and no item: the block could not go back as
/// it is.
#[test]
fn a_tool_use_whose_input_is_not_an_object_keeps_its_call() {
    let response = whole(json!({
        "stop_reason": "tool_use",
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": [1]}]
    }))
    .expect("the reply decodes");
    let [AssistantPart::ToolCall(call)] = response.message.content.as_slice() else {
        panic!("one call: {:?}", response.message.content);
    };
    assert_eq!(call.input.raw(), "{}");
    assert_eq!(call.invalid.as_deref(), Some("[1]"));
    assert!(item(&response.message.content[0]).is_none());
}

/// A blank text block keeps no item, so it can never go back as it is.
#[test]
fn a_blank_text_block_keeps_no_item() {
    let response = whole(json!({"stop_reason": "end_turn", "content": [
        {"type": "text", "text": "  "}, {"type": "text", "text": "real answer"}]}))
    .expect("the reply decodes");
    assert!(item(&response.message.content[0]).is_none());
    assert!(item(&response.message.content[1]).is_some());
}

/// Thinking that ended without a signature keeps no item, so it replays as
/// text; Kimi, which sends thinking unsigned, takes it back as thinking.
#[test]
fn unsigned_thinking_keeps_its_item_only_where_the_host_takes_it() {
    let unsigned = json!({"type": "thinking", "thinking": "plan", "signature": ""});
    for (host, model, kept) in [
        ("anthropic", SONNET_4_6, false),
        ("moonshot", "kimi-k2.6", true),
    ] {
        let provider = provider(host);
        let turn = turn_on(
            &provider,
            model,
            json!({"stop_reason": "max_tokens", "content": [unsigned.clone()]}),
        );
        assert_eq!(item(&turn.content[0]).is_some(), kept, "{model}");
        let body = sent_with(
            &provider,
            model,
            vec![
                Message::user("go"),
                Message::Assistant(turn),
                Message::user("next"),
            ],
            Vec::new(),
        );
        let replayed = &body["messages"][1]["content"][0];
        if kept {
            assert_eq!(replayed, &unsigned);
        } else {
            assert_eq!(replayed, &json!({"type": "text", "text": "plan"}));
        }
    }
}

/// A `tool_use` whose block states no `input` (a gateway's zero-argument
/// call) keeps an item that does; input that is not an object leaves the
/// call without an item, rebuilt from its arguments.
#[test]
fn a_kept_tool_use_item_always_states_an_object_input() {
    let provider = provider("zai");
    for (call, kept) in [
        (
            json!({"type": "tool_use", "id": "call_1", "name": "add"}),
            Some(json!({"type": "tool_use", "id": "call_1", "name": "add", "input": {}})),
        ),
        (
            json!({"type": "tool_use", "id": "call_1", "name": "add", "input": null}),
            Some(json!({"type": "tool_use", "id": "call_1", "name": "add", "input": {}})),
        ),
        (
            json!({"type": "tool_use", "id": "call_1", "name": "add", "input": "{\"x\":1}"}),
            None,
        ),
    ] {
        let turn = turn_on(
            &provider,
            "glm-4.6",
            json!({"stop_reason": "tool_use", "content": [call]}),
        );
        assert_eq!(item(&turn.content[0]).as_ref(), kept.as_ref());
        let body = sent_with(
            &provider,
            "glm-4.6",
            vec![
                Message::user("add"),
                Message::Assistant(turn),
                answer("call_1", "2"),
            ],
            vec![tool("add")],
        );
        assert!(
            body["messages"][1]["content"][0]["input"].is_object(),
            "{body}"
        );
    }
}

/// A `tool_use` sent without an id gets one the writer issues, and the
/// replayed item carries it, so the call and its result agree.
#[test]
fn an_idless_call_replays_under_the_id_its_result_gets() {
    for host in ["anthropic", "minimax"] {
        let provider = provider(host);
        for call in [
            json!({"type": "tool_use", "id": "", "name": "add", "input": {"x": 1}}),
            json!({"type": "tool_use", "name": "add", "input": {"x": 1}}),
        ] {
            let turn = turn_on(
                &provider,
                SONNET_4_6,
                json!({"stop_reason": "tool_use", "content": [call]}),
            );
            let id = turn.tool_calls().next().expect("a call").id.clone();
            assert!(id.starts_with("call_"), "{id}");
            let body = sent_with(
                &provider,
                SONNET_4_6,
                vec![
                    Message::user("add"),
                    Message::Assistant(turn),
                    answer(&id, "2"),
                ],
                vec![tool("add")],
            );
            let use_id = &body["messages"][1]["content"][0]["id"];
            assert_eq!(
                use_id, &body["messages"][2]["content"][0]["tool_use_id"],
                "{body}"
            );
            assert_eq!(use_id, &json!(id), "{body}");
        }
    }
}

const PROGRESS_UPDATE_TEXT: &str = "Confirmed the retry path never refreshes the expired token. Editing auth.py to add the refresh call.";

/// Progress-update thinking blocks, in the documented shape of a turn that
/// answers a `tool_result` on Claude Opus 5.5: a reasoning block, a
/// progress-update block, then the `tool_use` it introduces.
fn progress_update_content(update: &str) -> Value {
    json!([
        {"type": "thinking", "thinking": "", "signature": "EqMBCkYICxIM-reasoning"},
        {"type": "thinking", "thinking": update, "signature": "Es8CCkYICxIM-update"},
        {"type": "tool_use", "id": "toolu_01D7FLrfh4GYq7yT1ULFeyMV", "name": "edit_file",
            "input": {"path": "auth.py", "content": "..."}}
    ])
}

/// Both thinking blocks survive decoding unmerged and in order, whole and
/// streamed, and the turn replays to its model exactly as received.
#[test]
fn progress_update_blocks_survive_decoding_and_replay() {
    let provider = provider("anthropic");
    for update in ["", PROGRESS_UPDATE_TEXT] {
        let whole = whole_on(
            &provider,
            OPUS_5_5,
            json!({"stop_reason": "tool_use", "content": progress_update_content(update)}),
        )
        .expect("the message decodes");
        let thinking = |index, text: &str, signature: &str| {
            block(
                index,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
                &[
                    json!({"type": "thinking_delta", "thinking": text}),
                    json!({"type": "signature_delta", "signature": signature}),
                ],
            )
        };
        let mut frames = reply(
            vec![
                thinking(0, "", "EqMBCkYICxIM-reasoning"),
                thinking(1, update, "Es8CCkYICxIM-update"),
                block(
                    2,
                    json!({"type": "tool_use", "id": "toolu_01D7FLrfh4GYq7yT1ULFeyMV",
                        "name": "edit_file", "input": {}}),
                    &[json!({"type": "input_json_delta",
                        "partial_json": "{\"path\": \"auth.py\", \"content\": \"...\"}"})],
                ),
            ],
            "tool_use",
        );
        frames.push(json!({"type": "message_stop"}));
        let stream = decode_on(&provider, OPUS_5_5, &frames).expect("the stream decodes");
        for response in [whole, stream] {
            let items: Vec<Value> = response.message.content.iter().filter_map(item).collect();
            assert_eq!(Value::Array(items), progress_update_content(update));
            let mut edit_file = tool("edit_file");
            edit_file.description = "Edit a file.".to_owned();
            let body = sent_with(
                &provider,
                OPUS_5_5,
                vec![
                    Message::user("The login test fails after an hour of uptime."),
                    Message::Assistant(response.message),
                    answer("toolu_01D7FLrfh4GYq7yT1ULFeyMV", "saved"),
                ],
                vec![edit_file],
            );
            assert_eq!(
                body["messages"][1]["content"],
                progress_update_content(update)
            );
        }
    }
}

/// The container a code-execution reply ran in is a last opaque part that
/// names it, and the reply's other blocks replay around it.
#[test]
fn the_container_is_kept_as_a_last_opaque_part() {
    let response = whole(json!({
        "stop_reason": "end_turn",
        "container": {"id": "container_1", "expires_at": "2026-10-03T00:00:00Z"},
        "content": [{"type": "text", "text": "ran"}]
    }))
    .expect("the reply decodes");
    let last = response.message.content.last().expect("a part");
    assert_eq!(
        item(last).as_ref(),
        Some(&json!({"type": "container",
            "container": {"id": "container_1", "expires_at": "2026-10-03T00:00:00Z"}}))
    );
}

/// `frames` decoded by the decoder (`fast`), or each through a `Value` by
/// the decoder it replaced ([`super::oracle`]); `each` sees each frame's
/// flow and events.
fn decode_frames(
    frames: &[String],
    fast: bool,
    mut each: impl FnMut(Result<crate::Flow, Error>, &mut Vec<Event>),
) {
    let mut decoder = super::super::decode::MessagesDecoder::new(false);
    let mut oracle = oracle::Oracle::new(false);
    let mut events = Vec::new();
    for data in frames {
        let flow = if fast {
            Decoder::frame(&mut decoder, Frame { event: "", data }, &mut events)
        } else {
            // Every frame through its `Value` path.
            let flow = match data.trim() {
                "[DONE]" => Ok(crate::Flow::More),
                _ => oracle.value(data),
            };
            crate::writer::Writes::writer(&mut oracle).drain(&mut events);
            flow
        };
        each(flow, &mut events);
        events.clear();
    }
}

/// What each of `frames` decodes to.
fn events_of(frames: &[String], fast: bool) -> Vec<String> {
    let mut log = Vec::new();
    decode_frames(frames, fast, |flow, events| {
        log.push(format!("{flow:?} {events:?}"));
    });
    log
}

/// Frames the decoder must agree on with the `Value` path it replaced:
/// escapes, extra fields, empty and missing fragments, a string index, a
/// duplicate field, deltas to a block that never started and to one of
/// another kind.
#[test]
fn frames_read_once_decode_as_the_value_path_does() {
    let start = |index: usize, block: Value| {
        json!({"type": "content_block_start", "index": index, "content_block": block}).to_string()
    };
    let mut frames = vec![
        json!({"type": "message_start", "message": {"id": "m", "usage": {}}}).to_string(),
        start(0, json!({"type": "thinking", "thinking": ""})),
        start(
            1,
            json!({"type": "tool_use", "id": "toolu_1", "name": "f", "input": {}}),
        ),
        start(
            2,
            json!({"type": "server_tool_use", "id": "srv", "name": "s"}),
        ),
    ];
    for (index, delta) in [
        (
            json!(0),
            json!({"type": "thinking_delta", "thinking": "plain"}),
        ),
        (
            json!(0),
            json!({"type": "thinking_delta", "thinking": "esc\"aped\n\u{e9}"}),
        ),
        (json!(0), json!({"type": "thinking_delta", "thinking": ""})),
        (
            json!(0),
            json!({"type": "thinking_delta", "thinking": "x", "extra": [1]}),
        ),
        (
            json!(0),
            json!({"type": "thinking_delta", "thinking": "x", "signature": "s"}),
        ),
        (
            json!(0),
            json!({"type": "signature_delta", "signature": "sig\\"}),
        ),
        (json!(0), json!({"type": "signature_delta"})),
        (
            json!("0"),
            json!({"type": "thinking_delta", "thinking": "string index"}),
        ),
        (
            json!(1),
            json!({"type": "input_json_delta", "partial_json": "{\"a\":"}),
        ),
        (
            json!(1),
            json!({"type": "input_json_delta", "partial_json": "\"\\u00e9\"}"}),
        ),
        (
            json!(2),
            json!({"type": "input_json_delta", "partial_json": "{}"}),
        ),
        (
            json!(3),
            json!({"type": "text_delta", "text": "opens block 3"}),
        ),
        (json!(3), json!({"type": "text_delta", "text": null})),
        (
            json!(0),
            json!({"type": "input_json_delta", "partial_json": "{}"}),
        ),
    ] {
        frames.push(
            json!({"type": "content_block_delta", "index": index, "delta": delta}).to_string(),
        );
    }
    frames.push(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"a","thinking":"b"}}"#
            .to_owned(),
    );
    frames.extend(
        (0..4).map(|index| json!({"type": "content_block_stop", "index": index}).to_string()),
    );
    frames.push(
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {}})
            .to_string(),
    );
    // Frames after one that fails are decoded too: each path must agree on
    // every frame, not only on the reply.
    assert_eq!(events_of(&frames, true), events_of(&frames, false));
}

/// A block's text lands on its item when it stops, unless a delta of
/// another kind or another delta's field touches that text first: the item
/// is the one the `Value` path assembled token by token.
#[test]
fn a_blocks_text_lands_on_its_item_as_each_token_did() {
    let start = |index: usize, block: Value| {
        json!({"type": "content_block_start", "index": index, "content_block": block}).to_string()
    };
    let delta = |index: usize, delta: Value| {
        json!({"type": "content_block_delta", "index": index, "delta": delta}).to_string()
    };
    let stop = |index: usize| json!({"type": "content_block_stop", "index": index}).to_string();
    let text = |text: &str| json!({"type": "text_delta", "text": text});
    let thinking = |text: &str| json!({"type": "thinking_delta", "thinking": text});
    let signature = json!({"type": "signature_delta", "signature": "sig"});
    let cases: Vec<Vec<String>> = vec![
        // Its own deltas only, from a block that states text and one that
        // states none.
        vec![
            start(0, json!({"type": "text", "text": "a"})),
            delta(0, text("b")),
            delta(0, text("c")),
            stop(0),
        ],
        vec![
            start(0, json!({"type": "text"})),
            delta(0, text("b")),
            stop(0),
        ],
        vec![
            start(0, json!({"type": "text", "text": 5})),
            delta(0, text("b")),
            delta(0, text("c")),
            stop(0),
        ],
        // Another kind's delta to a text block, and text to thinking.
        vec![
            start(0, json!({"type": "text", "text": ""})),
            delta(0, text("b")),
            delta(0, thinking("t")),
            delta(0, text("c")),
            stop(0),
        ],
        vec![
            start(0, json!({"type": "thinking", "thinking": "x"})),
            delta(0, thinking("y")),
            delta(0, text("z")),
            delta(0, thinking("w")),
            delta(0, signature.clone()),
            stop(0),
        ],
        // A delta of no known type stating the text field, a citation, and
        // a text delta stating other fields.
        vec![
            start(0, json!({"type": "text", "text": ""})),
            delta(0, text("b")),
            delta(0, json!({"type": "mystery", "text": "!", "n": 1})),
            delta(
                0,
                json!({"type": "citations_delta", "citation": {"url": "u"}}),
            ),
            delta(0, json!({"type": "text_delta", "text": "c", "extra": "e"})),
            stop(0),
        ],
        // The block opened again while open, deltas after it stopped, and
        // a thinking block that never stops.
        vec![
            start(0, json!({"type": "text", "text": ""})),
            delta(0, text("b")),
            start(0, json!({"type": "text", "text": "again"})),
            delta(0, text("c")),
            stop(0),
            delta(0, text("late")),
            start(1, json!({"type": "thinking", "thinking": ""})),
            delta(1, thinking("open")),
            delta(1, signature),
        ],
    ];
    let end = json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {}});
    for mut frames in cases {
        frames.push(end.to_string());
        assert_eq!(
            events_of(&frames, true),
            events_of(&frames, false),
            "{frames:#?}"
        );
    }
}

/// Every recorded Messages stream decodes as the `Value` path decoded it.
#[test]
fn recorded_streams_decode_as_the_value_path_did() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("cassettes") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "yaml") {
                out.push(path);
            }
        }
    }
    let root = format!(
        "{}/../../fixtures/rig/cassettes",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut files = Vec::new();
    walk(std::path::Path::new(&root), &mut files);
    let mut streams = 0;
    for file in files {
        let text = std::fs::read_to_string(&file).expect("a cassette");
        for doc in serde_yaml_ng::Deserializer::from_str(&text) {
            let doc: serde_yaml_ng::Value = serde::Deserialize::deserialize(doc).expect("yaml");
            if !doc["when"]["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("/messages"))
            {
                continue;
            }
            let body = doc["then"]["body"].as_str().unwrap_or_default();
            let events = crate::sse::events(&mut crate::sse::SseFramer::new(), body.as_bytes());
            let frames: Vec<String> = events.into_iter().map(|(_, data)| data).collect();
            assert_eq!(
                events_of(&frames, true),
                events_of(&frames, false),
                "{}",
                file.display()
            );
            streams += 1;
        }
    }
    assert!(streams > 30, "{streams}");
}

/// The frames of the recorded Anthropic stream `name`.
fn recorded(name: &str) -> Vec<String> {
    let path = format!(
        "{}/../../fixtures/rig/cassettes/anthropic/{name}.yaml",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(path).expect("the cassette");
    let cassette: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).expect("yaml");
    let body = cassette["then"]["body"].as_str().expect("a body");
    let events = crate::sse::events(&mut crate::sse::SseFramer::new(), body.as_bytes());
    events.into_iter().map(|(_, data)| data).collect()
}

/// The decoder reads every delta of a recorded stream without building a
/// `Value`, borrowing each fragment that needs no unescaping, and decodes
/// the same events as the `Value` path. An optimized build is faster too;
/// both timings are printed.
#[test]
fn a_delta_is_read_without_a_value() {
    use super::super::decode::frame;
    let frames: Vec<String> = [
        "effect_corpus/cancelled_stream",
        "corpus_endings/tool_call_delta_stop",
    ]
    .into_iter()
    .flat_map(recorded)
    .collect();
    let deltas: Vec<&String> = frames
        .iter()
        .filter(|data| data.contains(r#""type":"content_block_delta""#))
        .collect();
    assert!(deltas.len() > 500, "{}", deltas.len());
    let read: Vec<_> = deltas
        .iter()
        .filter_map(|data| frame(data).ok().flatten())
        .collect();
    assert_eq!(read.len(), deltas.len(), "every delta reads as a frame");
    let escaped = deltas.iter().filter(|data| data.contains('\\')).count();
    let owned = read
        .iter()
        .filter(|frame| {
            frame.delta.0.as_ref().is_some_and(|delta| {
                matches!(delta.streamed, Some((_, std::borrow::Cow::Owned(_))))
            })
        })
        .count();
    assert_eq!(owned, escaped, "only an escaped fragment is copied");
    assert_eq!(events_of(&frames, true), events_of(&frames, false));
    let time = |fast: bool| {
        (0..15)
            .map(|_| {
                let start = std::time::Instant::now();
                decode_frames(&frames, fast, |flow, events| {
                    let _ = std::hint::black_box((flow, events));
                });
                start.elapsed()
            })
            .min()
            .expect("rounds")
    };
    let (fast, slow) = (time(true), time(false));
    eprintln!(
        "{} frames, {} deltas ({owned} copied): typed {fast:?}, Value {slow:?} ({:.2}x)",
        frames.len(),
        deltas.len(),
        slow.as_secs_f64() / fast.as_secs_f64()
    );
    if !cfg!(debug_assertions) {
        assert!(fast < slow, "typed {fast:?}, Value {slow:?}");
    }
}
