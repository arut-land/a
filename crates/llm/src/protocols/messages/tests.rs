//! Ported from rig's Anthropic tests (`completion`, `streaming`, `wire` and
//! `options`), less citations, telemetry, the catalog and model listing.

mod cache;
mod decode;
mod encode;
mod hosts;
mod options;
mod tools;

use serde_json::{Value, json};

use super::*;
use crate::protocol::{Auth, Frame, Provider};
use crate::spec::{Assistant, AssistantPart, Event, Message, Response, Tool};
use crate::{Accumulator, Error};

const SONNET_4_6: &str = "claude-sonnet-4-6";
const OPUS_4_8: &str = "claude-opus-4-8";
const OPUS_5_5: &str = "claude-opus-5-5";
const SONNET_5_5: &str = "claude-sonnet-5-5";

/// The host `id` from [`HOSTS`], at a test URL.
fn provider(id: &str) -> Provider {
    let (host, quirks) = HOSTS
        .iter()
        .find(|(host, _)| host.id == id)
        .expect("a host");
    provider_with(host.id, *quirks)
}

fn provider_with(id: &'static str, quirks: Quirks) -> Provider {
    Provider::new(id, "https://api.test", Auth::None, Messages::new(quirks))
}

fn target<'a>(provider: &'a Provider, model: &'a str) -> Target<'a> {
    Target { provider, model }
}

/// A request to `model` holding `messages`, with room for 64 tokens.
fn request(model: &str, messages: Vec<Message>) -> Request {
    Request {
        model: model.into(),
        messages,
        generation: crate::Generation {
            max_output_tokens: std::num::NonZeroU32::new(64),
            ..crate::Generation::default()
        },
        ..Request::default()
    }
}

/// A body and its headers.
type Sent = (Value, Vec<(&'static str, String)>);

/// `request` prepared and encoded for `provider`.
fn encoded(provider: &Provider, request: &Request) -> Result<Sent, Error> {
    let target = target(provider, &request.model);
    let prepared = crate::prepare(request, &target);
    let encoded = provider.protocol.encode(&target, &prepared)?;
    Ok((encoded.body, encoded.headers))
}

/// The body `request` sends to `provider`.
fn sent(provider: &Provider, request: &Request) -> Value {
    encoded(provider, request).expect("the request encodes").0
}

/// The `anthropic-beta` header `request` sends to `provider`.
fn beta(provider: &Provider, request: &Request) -> Option<String> {
    encoded(provider, request)
        .expect("the request encodes")
        .1
        .into_iter()
        .find_map(|(name, value)| (name == "anthropic-beta").then_some(value))
}

/// A tool named `name` taking an empty object.
fn tool(name: &str) -> Tool {
    Tool {
        name: name.into(),
        description: format!("{name} description"),
        schema: json!({"type": "object", "properties": {}}),
        strict: None,
    }
}

/// The reply `frames` decode to on `provider`, its message stamped as
/// `model`'s from `provider`.
fn decode_on(provider: &Provider, model: &str, frames: &[Value]) -> Result<Response, Error> {
    let request = request(model, vec![Message::user("hello")]);
    let mut decoder = provider
        .protocol
        .decoder(&target(provider, model), &request);
    let mut events: Vec<Event> = Vec::new();
    let mut acc = Accumulator::new();
    let mut done = false;
    for frame in frames {
        let data = frame.to_string();
        let flow = decoder.frame(
            Frame {
                event: "",
                data: &data,
            },
            &mut events,
        );
        for event in events.drain(..) {
            acc.push(event)?;
        }
        if flow? == crate::Flow::Done {
            done = true;
            break;
        }
    }
    if !done {
        let ended = decoder.end(&mut events);
        for event in events.drain(..) {
            acc.push(event)?;
        }
        ended?;
    }
    acc.finish(Some(crate::origin(provider, model)))
}

/// The reply `frames` decode to on Anthropic, for Claude Sonnet 4.6.
fn streamed(frames: &[Value]) -> Result<Response, Error> {
    decode_on(&provider("anthropic"), SONNET_4_6, frames)
}

/// The reply the whole message `message` decodes to on `provider`.
fn whole_on(provider: &Provider, model: &str, message: Value) -> Result<Response, Error> {
    let mut message = message;
    if let Some(fields) = message.as_object_mut() {
        fields.entry("type").or_insert_with(|| json!("message"));
        fields.entry("id").or_insert_with(|| json!("msg_1"));
        fields.entry("model").or_insert_with(|| json!(model));
        fields
            .entry("usage")
            .or_insert_with(|| json!({"input_tokens": 1, "output_tokens": 1}));
    }
    decode_on(provider, model, &[message])
}

fn whole(message: Value) -> Result<Response, Error> {
    whole_on(&provider("anthropic"), SONNET_4_6, message)
}

/// The turn `message` decodes to on `provider` for `model`.
fn turn_on(provider: &Provider, model: &str, message: Value) -> Assistant {
    whole_on(provider, model, message)
        .expect("the reply decodes")
        .message
}

/// The frames of one content block: its start, its deltas and its stop.
fn block(index: usize, start: Value, deltas: &[Value]) -> Vec<Value> {
    let mut opened = json!({"type": "content_block_start", "index": index});
    opened["content_block"] = start;
    std::iter::once(opened)
        .chain(
            deltas.iter().map(
                |delta| json!({"type": "content_block_delta", "index": index, "delta": delta}),
            ),
        )
        .chain([json!({"type": "content_block_stop", "index": index})])
        .collect()
}

/// A stream of `blocks` ending with `stop_reason`.
fn reply(blocks: Vec<Vec<Value>>, stop_reason: &str) -> Vec<Value> {
    std::iter::once(json!({"type": "message_start", "message": {
        "id": "msg_1", "type": "message", "role": "assistant", "model": SONNET_4_6,
        "content": [], "stop_reason": null, "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 1}
    }}))
    .chain(blocks.into_iter().flatten())
    .chain([json!({"type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
        "usage": {"output_tokens": 5}})])
    .collect()
}

/// The provider item a decoded part holds.
fn item(part: &AssistantPart) -> Option<&Value> {
    part.meta()
        .get(KEY)
        .and_then(|state| state.get("item"))
        .filter(|item| !item.is_null())
}

/// Whether the opaque `part` replays.
fn replays(part: &AssistantPart) -> bool {
    crate::replay::replays(part, KEY)
}

/// `history` sent to `model` on `provider` with `tools`, prepared as a turn
/// sends it.
fn sent_with(provider: &Provider, model: &str, history: Vec<Message>, tools: Vec<Tool>) -> Value {
    let mut request = request(model, history);
    request.tools = tools;
    sent(provider, &request)
}

/// The result answering call `id`.
fn answer(id: &str, text: &str) -> Message {
    Message::Tool(crate::ToolMessage {
        results: vec![crate::ToolResult {
            id: id.into(),
            name: "add".into(),
            output: crate::Output::Text(text.to_owned()),
            error: false,
            meta: crate::Meta::new(),
        }],
    })
}
