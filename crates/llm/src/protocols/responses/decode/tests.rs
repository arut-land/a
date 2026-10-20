use std::path::Path;

use serde_json::{Value, json};

use super::*;
use crate::accumulate::Accumulator;
use crate::spec::{AssistantPart, CallEnd, Ended, Event as Out, Response};

/// The frames of a recorded stream in `fixtures/rig/cassettes`.
fn recorded(cassette: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/rig/cassettes");
    let text = std::fs::read_to_string(path.join(cassette)).expect("cassette");
    let doc: serde_yaml_ng::Value = serde_yaml_ng::Deserializer::from_str(&text)
        .map(|doc| Deserialize::deserialize(doc).expect("yaml"))
        .next()
        .expect("an exchange");
    let body = doc["then"]["body"].as_str().expect("a body");
    let events = crate::sse::events(&mut crate::sse::SseFramer::new(), body.as_bytes());
    events.into_iter().map(|(_, data)| data).collect()
}

/// The events `frames` decode to, up to the frame that ends the reply.
fn events(frames: &[String]) -> Vec<Out> {
    let mut decoder = ResponsesDecoder::new();
    let mut out = Vec::new();
    for data in frames {
        let flow = decoder.data(data);
        decoder.writer().drain(&mut out);
        if flow.expect("a frame") == Flow::Done {
            break;
        }
    }
    out
}

/// What each of `frames` decodes to, failures included.
fn outcomes(frames: &[String]) -> Vec<Result<(Flow, Vec<Out>), Error>> {
    let mut decoder = ResponsesDecoder::new();
    frames
        .iter()
        .map(|data| {
            let flow = decoder.data(data);
            let mut out = Vec::new();
            decoder.writer().drain(&mut out);
            flow.map(|flow| (flow, out))
        })
        .collect()
}

/// Frames whose fields come in any order, of another type, twice, or not
/// at all are read leniently: a retyped index or delta is skipped, a
/// repeated key's last value stands, an item id addresses the item, and
/// JSON that is no object or names no known type says nothing.
#[test]
fn odd_frames_are_read_leniently() {
    let frames: Vec<String> = [
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "m1"}}).to_string(),
        r#"{"delta":"a","item_id":"m1","output_index":0,"type":"response.output_text.delta"}"#.to_owned(),
        r#"{"type":"response.output_text.delta","output_index":"0","delta":"b"}"#.to_owned(),
        r#"{"type":"response.output_text.delta","output_index":0,"delta":5}"#.to_owned(),
        r#"{"type":"response.output_text.delta","delta":"c","delta":"d","item_id":"m1"}"#.to_owned(),
        r#"{"type":"response.output_text.delta","item":{"id":"m1"},"delta":"e"}"#.to_owned(),
        r#"{"type":"response.output_text.delta","item":"m1","item_id":"m1","delta":"f"}"#.to_owned(),
        r#"{"type":"response.output_text.done","text":"abdef","output_index":0}"#.to_owned(),
        r#"{"type":5,"delta":"g"}"#.to_owned(),
        r#"["not", "an", "object"]"#.to_owned(),
        r#"{"type":"response.reasoning_summary_text.delta","output_index":1,"summary_index":1,"delta":"r"}"#.to_owned(),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "m1", "content": [{"type": "output_text", "text": "abdef"}]}}).to_string(),
        r#"{"type":"response.output_item.done","item":null}"#.to_owned(),
        json!({"type": "response.completed", "response": {"status": "completed", "usage": {"input_tokens": "3", "output_tokens": 2}}}).to_string(),
    ]
    .into();
    let read = outcomes(&frames);
    assert!(read.iter().all(Result::is_ok), "{read:?}");
    let mut acc = Accumulator::new();
    for event in events(&frames) {
        acc.push(event).expect("in order");
    }
    let reply = acc.finish(None).expect("a reply");
    assert_eq!(reply.message.text(), "abdef");
    let reasoning: Vec<&str> = reply
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantPart::Reasoning(reasoning) => Some(reasoning.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, ["r"]);
    assert_eq!((reply.usage.input, reply.usage.output), (Some(3), Some(2)));
    for failure in [
        json!({"error": {"code": "x", "message": "bad"}}),
        json!({"type": "response.failed", "response": {"error": {"code": "c", "message": "m"}}}),
    ] {
        let [outcome] = outcomes(&[failure.to_string()]).try_into().expect("one");
        let error = outcome.expect_err("the frame fails the reply");
        assert!(
            error.to_string().contains("bad") || error.to_string().contains('m'),
            "{error}"
        );
    }
}

fn reply(frames: &[Value]) -> Response {
    let frames: Vec<String> = frames.iter().map(Value::to_string).collect();
    let mut accumulator = Accumulator::new();
    for event in events(&frames) {
        accumulator.push(event).expect("an event");
    }
    accumulator.finish(None).expect("a reply")
}

const STREAMS: [&str; 3] = [
    "openai/corpus_breadth/text_delta_stop.yaml",
    "openai/streaming_grammar/parallel_tool_calls.yaml",
    "openai/reasoning_delta_hook/streaming.yaml",
];

/// Every recorded delta frame is read without a `Value`, borrowing its
/// fields: only a delta that holds an escape is copied, unescaped, and that
/// text is what its event owns anyway.
#[test]
fn every_recorded_delta_frame_is_read_in_place() {
    let frames: Vec<String> = STREAMS.iter().flat_map(|stream| recorded(stream)).collect();
    let (mut deltas, mut copied) = (0, 0);
    for data in &frames {
        let frame = frame(data).expect("a frame").expect("an object");
        let Json::Str(text) = &frame.delta else {
            assert!(!data.contains(".delta\""), "{data}");
            continue;
        };
        deltas += 1;
        if let Cow::Owned(text) = text {
            assert!(data.contains('\\'), "{data}");
            assert_eq!(
                serde_json::to_string(text)
                    .ok()
                    .map(|quoted| data.contains(&quoted[1..quoted.len() - 1])),
                Some(true)
            );
            copied += 1;
        }
    }
    assert!(deltas > 400, "{deltas}");
    eprintln!("{deltas} delta frames, {copied} with an escape copied");
}

/// A delta frame with a field of another type (an index sent as a string)
/// is read leniently, to the same effect.
#[test]
fn a_retyped_delta_field_is_read_leniently() {
    let added = json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1"}});
    let strict = json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hi"});
    let retyped = json!({"type": "response.output_text.delta", "output_index": "0", "delta": "Hi"});
    let completed = json!({"type": "response.completed", "response": {"status": "completed"}});
    let text = |delta: &Value| {
        reply(&[added.clone(), delta.clone(), completed.clone()])
            .message
            .text()
    };
    assert_eq!(text(&strict), "Hi");
    assert_eq!(text(&retyped), "Hi");
}

/// A custom call's raw input streams as the JSON object its arguments are
/// (rig's `custom_tool_input_streams_as_its_json_object`).
#[test]
fn custom_tool_input_streams_as_its_json_object() {
    let frames: Vec<String> = [
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "patch", "input": ""}}),
        json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "delta": "say \"hi\""}),
        json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "delta": "\n"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "patch", "input": "say \"hi\"\n"}}),
        json!({"type": "response.completed", "response": {"status": "completed"}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect();
    let events = events(&frames);
    let deltas: String = events
        .iter()
        .filter_map(|event| match event {
            Out::Delta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, r#"{"input":"say \"hi\"\n"}"#);
    let call = events.iter().find_map(|event| match event {
        Out::End {
            ended: Ended::Call(CallEnd { id, input, .. }),
            ..
        } => Some((id.clone(), input.clone())),
        _ => None,
    });
    let (id, input) = call.expect("the call ends");
    assert_eq!(id, "call_1");
    assert_eq!(input.raw(), r#"{"input":"say \"hi\"\n"}"#);
}

/// Without output indices, items are told apart by id and by order, and
/// the reply is the one the indexed stream makes (rig's
/// `a_stream_without_output_indices_agrees_with_its_whole_reply`).
#[test]
fn a_stream_without_output_indices_decodes_as_with_them() {
    for stream in STREAMS {
        let frames: Vec<Value> = recorded(stream)
            .iter()
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect();
        let unindexed: Vec<Value> = frames
            .iter()
            .cloned()
            .map(|mut frame| {
                frame
                    .as_object_mut()
                    .map(|fields| fields.remove("output_index"));
                frame
            })
            .collect();
        let (indexed, unindexed) = (reply(&frames), reply(&unindexed));
        assert_eq!(
            indexed.message.content, unindexed.message.content,
            "{stream}"
        );
        assert_eq!(indexed.finish, unindexed.finish, "{stream}");
    }
}

/// Reasoning is complete only once the item after it is, and a summary's
/// parts are paragraphs.
#[test]
fn reasoning_waits_for_the_item_it_precedes() {
    let summary = |index: u64, delta: &str| json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": index, "delta": delta});
    let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": []});
    let message = json!({"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": "Done."}]});
    let mut frames = vec![
        json!({"type": "response.output_item.added", "output_index": 0, "item": reasoning}),
        summary(0, "First."),
        summary(1, "Second."),
        json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": message}),
    ];
    let completed = json!({"type": "response.completed", "response": {"status": "completed"}});
    let held = reply(&[frames.clone(), vec![completed.clone()]].concat());
    let AssistantPart::Reasoning(part) = &held.message.content[0] else {
        panic!("{:?}", held.message.content);
    };
    assert_eq!(part.text, "First.\n\nSecond.");
    // The message never completed, so neither did the reasoning before it.
    assert!(part.meta.is_empty(), "{:?}", part.meta);
    frames.push(json!({"type": "response.output_item.done", "output_index": 1, "item": message}));
    let done = reply(&[frames, vec![completed]].concat());
    assert!(!done.message.content[0].meta().is_empty());
    assert_eq!(done.message.text(), "Done.");
}

/// `response.incomplete` ends the reply incomplete whatever its `status`
/// says, keeping the output so far.
#[test]
fn response_incomplete_is_incomplete_whatever_its_status_says() {
    let frames = [
        json!({"type": "response.output_text.delta", "delta": "Par"}),
        json!({"type": "response.incomplete", "response": {"status": "completed", "incomplete_details": {"reason": "max_output_tokens"}}}),
    ];
    let reply = reply(&frames);
    assert_eq!(reply.message.text(), "Par");
    assert_eq!(reply.finish.reason, Reason::Length);
    assert_eq!(reply.finish.raw.as_deref(), Some("max_output_tokens"));
}
