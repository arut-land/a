//! Ported from rig's Gemini `GenerateContent` tests (`completion/tests.rs`,
//! `streaming/tests.rs`, `options/tests.rs`), in this crate's terms.

use std::num::NonZeroU32;

use serde_json::{Value, json};

use super::encode::{NO_OUTPUT, RESULT_IMAGES, contents, drop_finished_signatures};
use super::*;
use crate::json::Lenient;
use crate::protocol::KeyStyle;
use crate::protocol::{Auth, Flow, Frame, Provider};
use crate::spec::{
    Assistant, AssistantPart, Effort, Format, Generation, Media, Message, Meta, Output, Reason,
    Reasoning, Response, Source, Text, Tool, ToolCall, ToolMessage, ToolResult, User, UserPart,
};
use crate::{Accumulator, Kind};

fn provider() -> Provider {
    Provider::new(
        "gemini",
        "https://example.test",
        Auth::None,
        Gemini::default(),
    )
}

/// The reply `chunks` add up to, each one SSE frame, as `model` produced it.
fn decoded_by(model: &str, chunks: &[Value]) -> Result<Response, Error> {
    let provider = provider();
    let request = Request::default();
    let target = Target {
        provider: &provider,
        model,
    };
    let mut decoder = provider.protocol.decoder(&target, &request);
    let mut events = Vec::new();
    let mut done = false;
    for chunk in chunks {
        let data = chunk.to_string();
        let frame = Frame {
            event: "",
            data: &data,
        };
        if decoder.frame(frame, &mut events)? == Flow::Done {
            done = true;
            break;
        }
    }
    if !done {
        decoder.end(&mut events)?;
    }
    let mut acc = Accumulator::new();
    for event in events {
        acc.push(event)?;
    }
    acc.finish(Some(crate::origin(&provider, model)))
}

fn decoded(chunks: &[Value]) -> Result<Response, Error> {
    decoded_by("gemini-2.5-flash", chunks)
}

/// One chunk holding `parts`, and `finish` when it ends the reply.
fn chunk(parts: Value, finish: Option<&str>) -> Value {
    let mut candidate = json!({"content": {"role": "model"}, "index": 0});
    candidate["content"]["parts"] = parts;
    if let Some(finish) = finish {
        candidate["finishReason"] = json!(finish);
    }
    json!({"candidates": [candidate], "modelVersion": "m", "responseId": "r"})
}

/// The body `request` sends `model`, prepared as the client prepares it.
fn sent(model: &str, request: Request) -> Value {
    let provider = provider();
    let request = Request {
        model: model.into(),
        ..request
    };
    let target = Target {
        provider: &provider,
        model,
    };
    let request = crate::prepare::prepare(&request, &target);
    provider
        .protocol
        .encode(&target, &request)
        .expect("the request encodes")
        .json()
}

fn asking(messages: Vec<Message>) -> Request {
    Request {
        messages: messages.into(),
        ..Request::default()
    }
}

/// The body that continues `response` from `model`: its calls answered
/// with `ok`, or a new user turn.
fn replayed(model: &str, response: &Response) -> Value {
    let calls: Vec<ToolCall> = response.message.tool_calls().cloned().collect();
    let mut messages = vec![
        Message::user("q"),
        Message::Assistant(response.message.clone()),
    ];
    if calls.is_empty() {
        messages.push(Message::user("n"));
    } else {
        messages.push(Message::Tool(ToolMessage {
            results: calls
                .iter()
                .map(|call| result(call, Output::Text("ok".into())))
                .collect(),
        }));
    }
    sent(model, asking(messages))
}

fn result(call: &ToolCall, output: Output) -> ToolResult {
    ToolResult {
        id: call.id.clone(),
        name: call.name.clone(),
        output,
        error: false,
        meta: Meta::new(),
    }
}

fn tool_result(id: &str, name: &str, output: Output) -> Message {
    Message::Tool(ToolMessage {
        results: vec![ToolResult {
            id: id.into(),
            name: name.into(),
            output,
            error: false,
            meta: Meta::new(),
        }],
    })
}

/// `message` as `model` gets it, with no preparation.
fn contents_for(model: &str, messages: &[Message]) -> Vec<Value> {
    contents(messages, model).expect("the history encodes").0
}

fn image(media_type: &str, data: &str) -> UserPart {
    UserPart::Media(Media {
        media_type: media_type.into(),
        source: Source::Data { data: data.into() },
        meta: Meta::new(),
    })
}

fn native(text: &str, item: Value) -> AssistantPart {
    let fp = crate::replay::fingerprint(crate::replay::Canon::Text(text));
    let mut part = Text::new(text);
    let mut state = json!({ "fp": fp });
    state["item"] = item;
    part.meta.insert(KEY.to_owned(), &state);
    AssistantPart::Text(part)
}

fn item(part: &AssistantPart) -> Option<Value> {
    part.meta().field(KEY, "item")
}

fn the_function_response(contents: &[Value]) -> &Value {
    contents
        .iter()
        .flat_map(|content| content.arr("parts"))
        .find_map(|part| part.get("functionResponse"))
        .expect("a function response")
}

mod decode;
mod encode;
mod hot_path;
mod options;
