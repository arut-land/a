use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;
use crate::accumulate::Accumulator;
use crate::spec::{AssistantPart, CallEnd, Event as Out, Response};

/// The frames of a recorded stream in `fixtures/rig/cassettes`.
fn recorded(cassette: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/rig/cassettes");
    let text = std::fs::read_to_string(path.join(cassette)).expect("cassette");
    let doc: serde_yaml_ng::Value = serde_yaml_ng::Deserializer::from_str(&text)
        .map(|doc| Deserialize::deserialize(doc).expect("yaml"))
        .next()
        .expect("an exchange");
    let body = doc["then"]["body"].as_str().expect("a body");
    let mut framer = crate::sse::SseFramer::new();
    framer
        .push(body.as_bytes())
        .map(|event| event.data)
        .collect()
}

/// The events `frames` decode to, `value` reading every frame as a `Value`.
fn events(frames: &[String], value: bool) -> Vec<Out> {
    let mut decoder = ResponsesDecoder::new();
    let mut out = Vec::new();
    for data in frames {
        let flow = if value {
            decoder.lenient(data)
        } else {
            decoder.data(data)
        };
        decoder.writer().drain(&mut out);
        if flow.expect("a frame") == Flow::Done {
            break;
        }
    }
    out
}

fn reply(frames: &[Value]) -> Response {
    let frames: Vec<String> = frames.iter().map(Value::to_string).collect();
    let mut accumulator = Accumulator::new();
    for event in events(&frames, false) {
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
/// text is what its event owns anyway. Other frames stop at their type.
#[test]
fn every_recorded_delta_frame_is_read_in_place() {
    let frames: Vec<String> = STREAMS.iter().flat_map(|stream| recorded(stream)).collect();
    let (mut deltas, mut copied) = (0, 0);
    for data in &frames {
        let Some(delta) = typed(data) else {
            assert!(!data.contains(".delta\""), "{data}");
            continue;
        };
        deltas += 1;
        if let Cow::Owned(text) = &delta.text {
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

/// The typed path decodes recorded streams exactly as the `Value` path
/// does, in about half the time (release: 220µs against 440µs for these
/// 480 frames), since it builds no tree and copies no field. Timing needs
/// an optimized build: `cargo nextest run --release -p llm --run-ignored
/// all -E 'test(faster)'`.
#[test]
#[cfg_attr(debug_assertions, ignore = "timing needs an optimized build")]
fn delta_frames_decode_as_values_do_but_faster() {
    let streams: Vec<Vec<String>> = STREAMS.iter().map(|stream| recorded(stream)).collect();
    for frames in &streams {
        assert_eq!(events(frames, false), events(frames, true));
    }
    let time = |value: bool| {
        let start = Instant::now();
        for frames in &streams {
            std::hint::black_box(events(frames, value));
        }
        start.elapsed()
    };
    // Interleaved, so both paths see the same load; the best run of each.
    let (mut typed, mut value) = (Duration::MAX, Duration::MAX);
    for _ in 0..30 {
        typed = typed.min(time(false));
        value = value.min(time(true));
    }
    eprintln!("typed {typed:?}, as values {value:?}");
    assert!(typed < value, "typed {typed:?}, as values {value:?}");
}

/// A delta frame whose fields do not fit the typed read (an index sent as a
/// string) is read leniently, to the same effect.
#[test]
fn a_retyped_delta_field_takes_the_value_path() {
    let added = json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1"}});
    let strict = json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hi"});
    let retyped = json!({"type": "response.output_text.delta", "output_index": "0", "delta": "Hi"});
    assert!(typed(&strict.to_string()).is_some());
    assert!(typed(&retyped.to_string()).is_none());
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
    let events = events(&frames, false);
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
            call: Some(CallEnd { id, input, .. }),
            ..
        } => Some((id.clone(), input.clone())),
        _ => None,
    });
    let (id, input) = call.expect("the call ends");
    assert_eq!(id, "call_1");
    assert_eq!(Value::Object(input), json!({"input": "say \"hi\"\n"}));
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
