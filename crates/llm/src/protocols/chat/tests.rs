//! Ported from rig's Chat wire tests: the stream's hard cases, the decoder
//! read from frames, and (in the modules below) the bodies requests and
//! replayed turns become.

mod bench;
mod history;
mod merge;
mod options;
mod request;

use serde_json::{Value, json};

use super::decode::finish_reason;
use super::*;
use crate::protocol::{Auth, Flow, Frame, Host, Provider};
use crate::spec::{AssistantPart, Block, Event, Message, Reason, Response};
use crate::{Accumulator, Kind};

pub(super) fn host(id: &str) -> (Host, Quirks) {
    *HOSTS
        .iter()
        .find(|(host, _)| host.id == id)
        .unwrap_or_else(|| panic!("no host {id}"))
}

/// The provider for host `id`, at its own base URL.
pub(super) fn provider(id: &str) -> Provider {
    let (host, quirks) = host(id);
    let base = if host.base_url.is_empty() {
        "https://example.openai.azure.com"
    } else {
        host.base_url
    };
    Provider::new(host.id, base, Auth::None, Chat::new(quirks))
}

/// A gateway with `quirks`.
pub(super) fn gateway(quirks: Quirks) -> Provider {
    Provider::new(
        "acme",
        "https://api.acme.test/v1",
        Auth::None,
        Chat::new(quirks),
    )
}

pub(super) const DONE: &str = "[DONE]";

/// A `chat.completion.chunk` whose primary choice carries `delta`.
pub(super) fn chunk(delta: Value, finish: Option<&str>) -> String {
    let mut chunk = json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "m",
        "choices": [{"index": 0, "finish_reason": finish}]});
    chunk["choices"][0]["delta"] = delta;
    chunk.to_string()
}

/// A whole `chat.completion`.
pub(super) fn whole(message: Value, finish: &str) -> String {
    let mut whole = json!({"id": "chatcmpl-1", "object": "chat.completion", "model": "m",
        "choices": [{"index": 0, "finish_reason": finish}]});
    whole["choices"][0]["message"] = message;
    whole.to_string()
}

fn text(text: &str) -> String {
    chunk(json!({"content": text}), None)
}

fn finish(reason: &str) -> String {
    chunk(json!({}), Some(reason))
}

/// A `tool_calls` fragment; an absent field is the wire's `null`.
fn call(index: u32, id: Option<&str>, name: Option<&str>, arguments: Option<&str>) -> String {
    chunk(
        json!({"tool_calls": [{"index": index, "id": id, "type": "function",
            "function": {"name": name, "arguments": arguments}}]}),
        None,
    )
}

/// `frames` through `provider`'s decoder for `model`: the events, and the
/// reply they fold to.
pub(super) fn stream(
    provider: &Provider,
    model: &str,
    frames: &[String],
) -> (Vec<Event>, Result<Response, Error>) {
    let request = Request {
        model: model.into(),
        ..Request::default()
    };
    let target = Target { provider, model };
    let mut decoder = provider.protocol.decoder(&target, &request);
    let mut events = Vec::new();
    let mut ended = Ok(());
    let mut done = false;
    for data in frames {
        match decoder.frame(
            Frame {
                event: "message",
                data,
            },
            &mut events,
        ) {
            Ok(Flow::More) => {}
            Ok(Flow::Done) => {
                done = true;
                break;
            }
            Err(error) => {
                ended = Err(error);
                done = true;
                break;
            }
        }
    }
    if !done {
        ended = decoder.end(&mut events);
    }
    let reply = ended.and_then(|()| {
        let mut acc = Accumulator::new();
        for event in &events {
            acc.push(event.clone())?;
        }
        acc.finish(Some(crate::origin(provider, model)))
    });
    (events, reply)
}

pub(super) fn decode(provider: &Provider, model: &str, frames: &[String]) -> Response {
    stream(provider, model, frames)
        .1
        .expect("the reply decodes")
}

/// The provider item a part holds.
pub(super) fn item(part: &AssistantPart) -> Option<&Value> {
    part.meta().get(KEY).and_then(|state| state.get("item"))
}

fn kinds(response: &Response) -> Vec<&'static str> {
    response
        .message
        .content
        .iter()
        .map(|part| match part {
            AssistantPart::Reasoning(_) => "reasoning",
            AssistantPart::Text(_) => "text",
            AssistantPart::ToolCall(_) => "call",
            _ => "other",
        })
        .collect()
}

/// `[DONE]` before any finish reason fails the turn: the provider never
/// said the turn ended.
#[test]
fn the_done_sentinel_without_a_finish_fails() {
    let (_, reply) = stream(&provider("openai"), "m", &[text("hi"), DONE.into()]);
    assert_eq!(reply.expect_err("never finished").kind, Kind::Incomplete);
}

/// With the quirk, `[DONE]` ends a text turn that named no reason as a
/// stop, and a turn holding a call as a call, delivered whole.
#[test]
fn the_done_quirk_ends_a_turn_at_the_sentinel() {
    let lenient = gateway(Quirks {
        done_without_finish_reason: true,
        ..Quirks::OPENAI
    });
    let response = decode(&lenient, "m", &[text("complete answer"), DONE.into()]);
    assert_eq!(response.finish.reason, Reason::Stop);
    assert_eq!(response.message.text(), "complete answer");

    let frames = [
        call(0, Some("call_1"), Some("read"), Some(r#"{"path""#)),
        call(0, None, None, Some(r#":"README.md"}"#)),
        DONE.into(),
    ];
    let response = decode(&lenient, "m", &frames);
    assert_eq!(response.finish.reason, Reason::ToolCalls);
    let [AssistantPart::ToolCall(call)] = response.message.content.as_slice() else {
        panic!("one call: {:?}", response.message.content);
    };
    assert_eq!(call.name, "read");
    assert_eq!(
        Value::Object(call.input.clone()),
        json!({"path": "README.md"})
    );
}

/// The quirk never turns an empty stream into an answer, nor ends a stream
/// that stops without the sentinel.
#[test]
fn the_done_quirk_still_fails_what_never_ended() {
    let lenient = gateway(Quirks {
        done_without_finish_reason: true,
        ..Quirks::OPENAI
    });
    for frames in [
        vec![DONE.to_owned()],
        vec!["\"noise\"".to_owned(), DONE.to_owned()],
        vec![text("hi")],
    ] {
        let (_, reply) = stream(&lenient, "m", &frames);
        assert_eq!(
            reply.expect_err("never ended").kind,
            Kind::Incomplete,
            "{frames:?}"
        );
    }
}

/// A stream that stops after its finish reason without `[DONE]` ended
/// (Perplexity never sends the sentinel).
#[test]
fn a_finished_stream_needs_no_sentinel() {
    let response = decode(
        &provider("perplexity"),
        "sonar",
        &[text("hi"), finish("stop")],
    );
    assert_eq!(response.message.text(), "hi");
    assert_eq!(response.finish.reason, Reason::Stop);
}

/// A key a frame repeats reads as its last value, at every level.
#[test]
fn a_repeated_key_reads_as_its_last_value() {
    let frames = [
        r#"{"choices":[{"index":0,"delta":{"content":"no","content":"yes"}}]}"#.to_owned(),
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","id":"c1","function":{"name":"x","name":"run","arguments":"{}"}}]}}]}"#.to_owned(),
        r#"{"choices":[{"index":0,"finish_reason":"stop","finish_reason":"tool_calls","delta":{}}]}"#.to_owned(),
    ];
    let response = decode(&provider("openai"), "m", &frames);
    assert_eq!(response.message.text(), "yes");
    let calls: Vec<_> = response.message.tool_calls().collect();
    assert_eq!(
        (calls[0].id.as_str(), calls[0].name.as_str()),
        ("c1", "run")
    );
    assert_eq!(response.finish.reason, Reason::ToolCalls);
}

/// The in-band error envelope arrives with a 200 status: it fails the turn
/// with the provider's message. A `null` or empty `error` beside terminal
/// usage, or any error beside content, is no failure.
#[test]
fn an_in_band_error_envelope_fails_the_turn() {
    let openai = provider("openai");
    for envelope in [
        r#"{"error":{"message":"rate limited","type":"rate_limit_error"},"choices":[]}"#,
        r#"{"error":{"message":"rate limited"},"choices":null}"#,
        r#"{"error":"rate limited"}"#,
    ] {
        let (_, reply) = stream(&openai, "m", &[envelope.into(), DONE.into()]);
        let error = reply.expect_err("the envelope fails the turn");
        assert!(error.message.contains("rate limited"), "{error:?}");
    }
    let (_, reply) = stream(
        &openai,
        "m",
        &[r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#.into()],
    );
    assert_eq!(reply.expect_err("rate limited").kind, Kind::RateLimit);
    for benign in [
        r#"{"error":null,"usage":{"total_tokens":3}}"#,
        r#"{"error":""}"#,
        r#"{"error":"metadata","choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
    ] {
        let response = decode(&openai, "m", &[benign.into(), finish("stop"), DONE.into()]);
        assert_eq!(response.finish.reason, Reason::Stop, "{benign}");
    }
}

/// Mira's gateway can answer with a bare JSON string: the whole answer,
/// ended as a stop. Elsewhere it is a frame this wire does not model.
#[test]
fn a_gateway_may_answer_with_a_bare_string() {
    let response = decode(
        &provider("mira"),
        "gpt-4o",
        &["\"the whole answer\"".into()],
    );
    assert_eq!(response.message.text(), "the whole answer");
    assert_eq!(response.finish.reason, Reason::Stop);
    assert_eq!(response.usage, crate::Usage::default());
    assert_eq!(response.id, None);
    let (_, strict) = stream(&provider("openai"), "m", &["\"the whole answer\"".into()]);
    assert_eq!(strict.expect_err("no answer").kind, Kind::Incomplete);
}

/// A call that never gets a name is dropped, since nothing can answer it; a
/// custom call is a call whose arguments are its `{"input"}`, kept with its
/// item; a call of a kind nothing can answer is kept but never sent back;
/// and content of no content type is nothing.
#[test]
fn malformed_and_unknown_calls_are_never_dropped_silently() {
    let openai = provider("openai");
    let nameless = chunk(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
            "function": {"arguments": "{}"}}]}),
        Some("tool_calls"),
    );
    let response = decode(&openai, "m", &[nameless, DONE.into()]);
    assert!(response.message.content.is_empty(), "{response:?}");

    let custom = chunk(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "custom",
            "custom": {"name": "grep", "input": "x"}}]}),
        Some("tool_calls"),
    );
    let response = decode(&openai, "m", &[custom, DONE.into()]);
    let [part @ AssistantPart::ToolCall(call)] = response.message.content.as_slice() else {
        panic!("a custom call is a call: {:?}", response.message.content);
    };
    assert_eq!(call.name, "grep");
    assert_eq!(Value::Object(call.input.clone()), json!({"input": "x"}));
    assert_eq!(item(part).map(|item| &item["type"]), Some(&json!("custom")));

    let invented = chunk(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "x_rig_invented",
            "x_rig_invented": {"name": "grep"}}]}),
        Some("tool_calls"),
    );
    let response = decode(&openai, "m", &[invented, DONE.into()]);
    let [part @ AssistantPart::Opaque(_)] = response.message.content.as_slice() else {
        panic!(
            "an unknown call kind is opaque: {:?}",
            response.message.content
        );
    };
    assert_eq!(
        item(part).map(|item| &item["type"]),
        Some(&json!("x_rig_invented"))
    );
    assert!(!crate::replay::replays(part, KEY));

    let numeric = chunk(json!({"content": 42}), Some("stop"));
    let response = decode(&openai, "m", &[numeric, DONE.into()]);
    assert!(response.message.content.is_empty(), "{response:?}");
}

/// A message's reasoning is one part, so a signature that arrives after the
/// answer joins the reasoning before it.
#[test]
fn a_late_signature_joins_the_reasoning_before_the_answer() {
    let signature = |signature: &str| {
        chunk(
            json!({"reasoning_details": [{"type": "reasoning.text", "text": "",
                "signature": signature, "index": 0}]}),
            None,
        )
    };
    let openrouter = provider("openrouter");
    let response = decode(
        &openrouter,
        "m",
        &[
            chunk(json!({"reasoning": "thinking"}), None),
            text("answer"),
            signature("sig-late"),
            finish("stop"),
            DONE.into(),
        ],
    );
    let [
        reasoning @ AssistantPart::Reasoning(first),
        AssistantPart::Text(answer),
    ] = response.message.content.as_slice()
    else {
        panic!(
            "the reasoning and the answer: {:?}",
            response.message.content
        );
    };
    assert_eq!(first.text, "thinking");
    assert_eq!(answer.text, "answer");
    let signed =
        json!([{"type": "reasoning.text", "text": "", "signature": "sig-late", "index": 0}]);
    assert_eq!(
        item(reasoning).map(|item| &item["reasoning_details"]),
        Some(&signed)
    );

    // A second signature for the same detail fills nothing.
    let response = decode(
        &openrouter,
        "m",
        &[
            chunk(json!({"reasoning": "thinking"}), None),
            signature("sig-1"),
            signature("sig-2"),
            text("answer"),
            finish("stop"),
            DONE.into(),
        ],
    );
    assert_eq!(
        response
            .message
            .content
            .first()
            .and_then(item)
            .map(|item| &item["reasoning_details"]),
        Some(&json!([{"type": "reasoning.text", "text": "", "signature": "sig-1", "index": 0}]))
    );
}

/// Reasoning that arrives after the answer is still the turn's first part,
/// while the events it streams keep their arrival order.
#[test]
fn late_reasoning_is_held_first_and_streamed_in_arrival_order() {
    let (events, reply) = stream(
        &provider("openrouter"),
        "m",
        &[
            text("answer"),
            chunk(json!({"reasoning": "late"}), None),
            finish("stop"),
            DONE.into(),
        ],
    );
    let response = reply.expect("the reply folds");
    assert_eq!(kinds(&response), ["reasoning", "text"]);
    let starts: Vec<Block> = events
        .iter()
        .filter_map(|event| match event {
            Event::Start { block, .. } => Some(*block),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [Block::Text, Block::Reasoning]);
}

/// `OpenRouter` streams its reasoning details as fragments; the reasoning part
/// keeps each detail's id, format and kind as sent, the encrypted entry
/// whole beside the merged summary.
#[test]
fn a_streamed_reasoning_detail_keeps_its_id_format_and_kind() {
    let summary = |text: &str| {
        chunk(
            json!({"reasoning": text, "reasoning_details": [{"type": "reasoning.summary",
                "format": "openai-responses-v1", "index": 0, "summary": text}]}),
            None,
        )
    };
    let response = decode(
        &provider("openrouter"),
        "m",
        &[
            summary("Weighing "),
            summary("it up"),
            chunk(
                json!({"reasoning_details": [{"type": "reasoning.encrypted", "id": "rs_1",
                    "format": "openai-responses-v1", "index": 0, "data": "gAAA"}]}),
                None,
            ),
            chunk(json!({"content": "done"}), Some("stop")),
            DONE.into(),
        ],
    );
    let Some(part @ AssistantPart::Reasoning(reasoning)) = response.message.content.first() else {
        panic!("the reasoning leads: {:?}", response.message.content);
    };
    assert_eq!(reasoning.text, "Weighing it up");
    let item = item(part).expect("the part keeps its fields");
    assert_eq!(
        item["reasoning_details"],
        json!([
            {"type": "reasoning.summary", "format": "openai-responses-v1", "index": 0,
                "summary": "Weighing it up"},
            {"type": "reasoning.encrypted", "id": "rs_1", "format": "openai-responses-v1",
                "index": 0, "data": "gAAA"},
        ])
    );
    assert_eq!(item["reasoning"], "Weighing it up");
}

#[test]
fn a_provider_id_sent_twice_is_renamed() {
    let response = decode(
        &provider("openai"),
        "m",
        &[
            call(0, Some("call_same"), Some("add"), Some(r#"{"a":1}"#)),
            call(1, Some("call_same"), Some("add"), Some(r#"{"a":2}"#)),
            finish("tool_calls"),
            DONE.into(),
        ],
    );
    let ids: Vec<&str> = response
        .message
        .tool_calls()
        .map(|call| call.id.as_str())
        .collect();
    let [first, second] = ids.as_slice() else {
        panic!("both calls are kept: {ids:?}");
    };
    assert_eq!(*first, "call_same");
    assert!(
        second.starts_with("call_") && *second != "call_same",
        "{second}"
    );
}

/// A streamed call with a `null` index and a numeric id is still the call,
/// and object content is a content part.
#[test]
fn a_mistyped_part_never_fails_a_reply() {
    let response = decode(
        &provider("openai"),
        "gpt-4.1-mini",
        &[
            chunk(
                json!({"role": "assistant", "content": {"type": "text", "text": "looking"}}),
                None,
            ),
            chunk(
                json!({"tool_calls": [{"index": null, "id": 42, "type": "function",
                    "function": {"name": "add", "arguments": "{\"x\":1}"}}]}),
                Some("tool_calls"),
            ),
            DONE.into(),
        ],
    );
    let [AssistantPart::Text(text), AssistantPart::ToolCall(call)] =
        response.message.content.as_slice()
    else {
        panic!("the text and the call: {:?}", response.message.content);
    };
    assert_eq!(text.text, "looking");
    assert_eq!(call.id, "42");
    assert_eq!(Value::Object(call.input.clone()), json!({"x": 1}));
}

/// Two whole calls streamed with neither an index nor an id are two calls:
/// a fragment continues the latest call only until its arguments are whole.
#[test]
fn index_less_id_less_calls_split_once_whole() {
    let call = |name: &str, arguments: &str| {
        chunk(
            json!({"tool_calls": [{"type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        )
    };
    let response = decode(
        &provider("openai"),
        "gpt-4o",
        &[
            chunk(json!({"role": "assistant"}), None),
            call("f", "{\"q\":"),
            chunk(
                json!({"tool_calls": [{"function": {"arguments": "\"a\"}"}}]}),
                None,
            ),
            call("f", "{\"q\":\"b\"}"),
            finish("tool_calls"),
            DONE.into(),
        ],
    );
    let arguments: Vec<Value> = response
        .message
        .tool_calls()
        .map(|call| Value::Object(call.input.clone()))
        .collect();
    assert_eq!(arguments, [json!({"q": "a"}), json!({"q": "b"})]);
}

/// A call the output budget cut keeps no item, and so replays from its
/// shared fields; a finished call and the reasoning the reply ended keep
/// theirs.
#[test]
fn only_a_complete_item_is_kept() {
    let deepseek = provider("deepseek");
    let reply = |arguments: &str, finish: &str| {
        decode(
            &deepseek,
            "deepseek-v4-flash",
            &[
                chunk(
                    json!({"role": "assistant", "reasoning_content": "plan"}),
                    None,
                ),
                chunk(
                    json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                        "function": {"name": "add", "arguments": arguments}}]}),
                    Some(finish),
                ),
                DONE.into(),
            ],
        )
    };
    let cut = reply(r#"{"x": 1"#, "length");
    let [reasoning, call @ AssistantPart::ToolCall(_)] = cut.message.content.as_slice() else {
        panic!("reasoning and the cut call: {:?}", cut.message.content);
    };
    assert!(item(reasoning).is_some(), "the reply ended the reasoning");
    assert!(item(call).is_none(), "the budget cut the call: {call:?}");
    let whole = reply(r#"{"x": 1}"#, "tool_calls");
    assert!(
        whole
            .message
            .content
            .iter()
            .all(|part| item(part).is_some())
    );
}

/// Usage never fails a reply: a mistyped counter is unreported, a details
/// object the usage has makes its counters zero, cached input falls back to
/// Mistral's and `DeepSeek`'s fields, and a host whose reasoning count cannot
/// be trusted reports none.
#[test]
fn usage_is_read_leniently() {
    let usage = |provider: &Provider, usage: Value| {
        let frames = [
            text("hi"),
            json!({"id": "c", "object": "chat.completion.chunk", "model": "m",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage})
            .to_string(),
            DONE.into(),
        ];
        decode(provider, "m", &frames).usage
    };
    let openai = provider("openai");
    let read = usage(
        &openai,
        json!({"prompt_tokens": 10, "completion_tokens": "7", "total_tokens": 17,
            "prompt_tokens_details": {}, "completion_tokens_details": {"reasoning_tokens": 3}}),
    );
    assert_eq!(read.input, Some(10));
    assert_eq!(read.output, Some(7), "the rest of the total");
    assert_eq!(read.cache_read, Some(0));
    assert_eq!(read.reasoning, Some(3));
    let read = usage(
        &provider("deepseek"),
        json!({"prompt_tokens": 10, "completion_tokens": 2, "prompt_cache_hit_tokens": 4}),
    );
    assert_eq!(read.cache_read, Some(4));
    let read = usage(
        &provider("doubleword"),
        json!({"prompt_tokens": 10, "completion_tokens": 2,
            "completion_tokens_details": {"reasoning_tokens": 30}}),
    );
    assert_eq!(read.reasoning, None);
}

/// A host states its own vocabulary on top of the shared one: Z.AI's
/// context-window stop is a length stop there and unknown elsewhere, and
/// every host shares the stop spellings compatible servers use.
#[test]
fn a_host_states_its_own_finish_vocabulary() {
    let zai = host("zai").1;
    let together = host("together").1;
    assert_eq!(
        finish_reason("model_context_window_exceeded", &zai),
        Reason::Length
    );
    assert_eq!(finish_reason("sensitive", &zai), Reason::ContentFilter);
    assert_eq!(
        finish_reason("model_context_window_exceeded", &Quirks::OPENAI),
        Reason::Other
    );
    for reason in ["stop", "end", "eos", "end_turn", "stop_sequence"] {
        for quirks in [&Quirks::OPENAI, &together, &zai] {
            assert_eq!(finish_reason(reason, quirks), Reason::Stop, "{reason}");
        }
    }
    let response = decode(
        &provider("zai"),
        "glm-5",
        &[text("x"), finish("sensitive"), DONE.into()],
    );
    assert_eq!(response.finish.reason, Reason::ContentFilter);
    assert_eq!(response.finish.raw.as_deref(), Some("sensitive"));
}

/// A gateway's upstream-native reason stands in only where the normalized
/// one is missing.
#[test]
fn a_native_finish_reason_fills_a_missing_one() {
    let frames = [
        text("x"),
        json!({"id": "c", "object": "chat.completion.chunk", "model": "m", "choices": [{
            "index": 0, "delta": {}, "finish_reason": null, "native_finish_reason": "MAX_TOKENS"}]})
        .to_string(),
        DONE.into(),
    ];
    let response = decode(&provider("openrouter"), "m", &frames);
    assert_eq!(response.finish.reason, Reason::Length);
    let (_, strict) = stream(&provider("openai"), "m", &frames);
    assert_eq!(strict.expect_err("no reason").kind, Kind::Incomplete);
}

/// An image the model produced is a media part: a data URL as its data.
#[test]
fn a_produced_image_is_media() {
    let response = decode(
        &provider("openrouter"),
        "google/gemini-2.5-flash-image",
        &[
            chunk(
                json!({"content": "here", "images": [{"type": "image_url",
                    "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}]}),
                Some("stop"),
            ),
            DONE.into(),
        ],
    );
    let [AssistantPart::Text(_), AssistantPart::Media(media)] = response.message.content.as_slice()
    else {
        panic!("the text and the image: {:?}", response.message.content);
    };
    assert_eq!(media.media_type, "image/png");
    assert_eq!(
        media.source,
        crate::Source::Data {
            data: "iVBORw0KGgo=".into()
        }
    );
}

/// The `[DONE]`-less whole reply in one frame decodes as the stream does.
#[test]
fn a_whole_reply_in_one_frame_ends_the_turn() {
    let response = decode(
        &provider("openai"),
        "m",
        &[whole(json!({"role": "assistant", "content": "hi"}), "stop")],
    );
    assert_eq!(response.message.text(), "hi");
    assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(response.model.as_deref(), Some("m"));
}

/// Foreign call ids are normalized the way pi does: a Responses
/// `call|item` id joins its legal halves, hashed past 40; `OpenAI` cuts any
/// other id to 40; other hosts keep theirs.
#[test]
fn foreign_call_ids_are_normalized() {
    let openai = Chat::new(host("openai").1);
    assert_eq!(openai.tool_id("call_abc123"), None);
    assert_eq!(
        openai
            .tool_id("toolu_01ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij")
            .as_deref(),
        Some("toolu_01ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef")
    );
    assert_eq!(openai.tool_id("a.b:c"), None);
    assert_eq!(
        openai.tool_id("call_1|fc+ab/c").as_deref(),
        Some("call_1_fc_ab_c")
    );
    assert_eq!(
        openai
            .tool_id("call_abcdefghijklmnopqrstuvwxyz0123|fc_0123456789abcdefghijklmnopqrstuvwxyz")
            .as_deref(),
        Some("call_abcdefghijklmnopqrstuvwxyz_7gi8bx11")
    );
    let deepseek = Chat::new(host("deepseek").1);
    assert_eq!(
        deepseek.tool_id("toolu_01ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"),
        None
    );
    let mistral = Chat::new(host("mistral").1);
    assert_eq!(mistral.tool_id("abc123XYZ"), None);
}

/// The turn `response` holds, as a message to send back.
pub(super) fn turn(response: &Response) -> Message {
    Message::Assistant(response.message.clone())
}

/// A request to `model` holding `messages`, declaring every tool they call,
/// as a tool loop does.
pub(super) fn request(model: &str, messages: Vec<Message>) -> Request {
    let mut tools: Vec<crate::Tool> = Vec::new();
    for message in &messages {
        let names: Vec<&str> = match message {
            Message::Assistant(assistant) => assistant
                .tool_calls()
                .map(|call| call.name.as_str())
                .collect(),
            Message::Tool(tool) => tool
                .results
                .iter()
                .map(|result| result.name.as_str())
                .collect(),
            Message::System(_) | Message::User(_) => Vec::new(),
        };
        for name in names {
            if !tools.iter().any(|tool| tool.name == name) {
                tools.push(crate::Tool {
                    name: name.into(),
                    description: "a tool the conversation calls".to_owned(),
                    schema: json!({"type": "object", "properties": {}}),
                    strict: None,
                });
            }
        }
    }
    Request {
        model: model.into(),
        messages,
        tools,
        ..Request::default()
    }
}

/// The body `request` sends to `provider`, prepared as the client prepares
/// it, with the URL it goes to.
pub(super) fn sent_to(provider: &Provider, request: &Request) -> Result<(String, Value), Error> {
    let target = Target {
        provider,
        model: &request.model,
    };
    let prepared = crate::prepare(request, &target);
    let http = provider.request(&request.model, &prepared)?;
    let body = serde_json::from_slice(http.body()).expect("a JSON body");
    Ok((http.uri().to_string(), body))
}

pub(super) fn sent(provider: &Provider, request: &Request) -> Value {
    sent_to(provider, request).expect("the request encodes").1
}
