//! A request as a Messages body. Turns this protocol produced replay their
//! provider blocks verbatim: signed thinking, redacted thinking, calls with
//! their `caller`, cited text, server-tool steps. A part edited since, or
//! from another origin, is rebuilt from its shared fields as pi rebuilds it:
//! thinking without its signature goes as text.

use std::num::NonZeroU32;

use serde_json::{Map, Value, json};

use super::cache::Caching;
use super::{
    Dialect, FINE_GRAINED_TOOL_STREAMING_BETA, KEY, Quirks, THINKING_BINDING_BETA,
    ToolInputStreaming, options, strict,
};
use crate::error::Error;
use crate::json::Lenient;
use crate::models::anthropic::{
    binds_context, rejects_forced_tool_choice, takes_mid_conversation_system,
};
use crate::replay::{Replay, replay, replays};
use crate::spec::{
    Assistant, AssistantPart, Format, Media, Message, Meta, Output, Request, Source, ToolChoice,
    ToolResult, UserPart,
};

/// The options the encoder reads rather than sends.
const READ: [&str; 4] = [
    "tools",
    "prompt_caching",
    "static_prefix_cache_ttl",
    "betas",
];

/// What an assistant image goes as: assistant turns take no images.
pub(super) const ASSISTANT_IMAGE_OMITTED: &str = "(image omitted: model does not support images)";

/// The keys of a stale item a rebuilt part keeps: the `caller` that ties a
/// call to the code execution that made it.
fn identity(item: &Value) -> Map<String, Value> {
    item.get("caller")
        .filter(|_| item.str("type") == Some("tool_use"))
        .map_or_else(Map::new, |caller| {
            Map::from_iter([("caller".to_owned(), caller.clone())])
        })
}

/// The body for `request` to `model`, and the `anthropic-beta` flags it
/// needs: the request's own fields, then the mapped generation settings,
/// then the caller's options, then the rewrites the API requires.
pub(super) fn body(
    quirks: Quirks,
    model: &str,
    request: &Request,
) -> Result<(Value, Vec<String>), Error> {
    let options = request
        .options
        .get(KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let max_tokens = request
        .generation
        .max_output_tokens
        .map(NonZeroU32::get)
        .or_else(|| quirks.default_max_tokens(model))
        .ok_or_else(|| {
            Error::unsupported(format!("`max_tokens` must be set for the model {model}"))
        })?;
    let mut body = base(quirks, model, request, &options, max_tokens)?;
    let mapped = options::generation(quirks, model, &request.generation, max_tokens);
    crate::json::overlay(&mut body, &mapped);
    let raw: Map<String, Value> = options
        .iter()
        .filter(|(key, _)| !READ.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    crate::json::overlay(&mut body, &raw);
    let unbound =
        quirks.dialect == Dialect::Anthropic && binds_context(model) && drop_unbound(&mut body);
    // Anthropic rejects a tool choice without tools.
    let has_tools = !body.arr("tools").is_empty();
    if let Some(fields) = body.as_object_mut() {
        if has_tools {
            fields
                .entry("tool_choice")
                .or_insert_with(|| json!({"type": "auto"}));
        } else {
            fields.remove("tool_choice");
        }
        fields.insert("stream".to_owned(), Value::Bool(true));
    }
    let fine_grained =
        !request.tools.is_empty() && quirks.tool_input_streaming == ToolInputStreaming::BetaHeader;
    let mut betas: Vec<String> = options
        .get("betas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    for (needed, beta) in [
        (unbound, THINKING_BINDING_BETA),
        (fine_grained, FINE_GRAINED_TOOL_STREAMING_BETA),
    ] {
        if needed && !betas.iter().any(|known| known == beta) {
            betas.push(beta.to_owned());
        }
    }
    Ok((body, betas))
}

/// The request's own fields: its messages, tools, tool choice, output
/// format and container, with the cache breakpoints placed inside them.
fn base(
    quirks: Quirks,
    model: &str,
    request: &Request,
    options: &Map<String, Value>,
    max_tokens: u32,
) -> Result<Value, Error> {
    let caching = Caching::of(options)?;
    let raw_tools = match options.get("tools") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(tools)) => tools.clone(),
        Some(_) => return Err(Error::unsupported("the `tools` option must be an array")),
    };
    let mut tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| tool_json(tool, quirks))
        .chain(raw_tools)
        .collect();
    let (mut system, turns) = split_system(&request.messages, takes_mid_conversation_system(model));
    let mut messages = Vec::with_capacity(turns.len());
    for turn in &turns {
        messages.extend(turn_json(turn)?);
    }
    caching.place(&mut system, &mut messages, &mut tools)?;
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
    });
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if let Some(choice) = tool_choice(quirks.dialect, model, request, !tools.is_empty()) {
        body["tool_choice"] = choice;
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(Format::Json {
        schema: Some(schema),
        ..
    }) = &request.format
    {
        let mut schema = schema.clone();
        crate::protocols::schema::sanitize(
            &mut schema,
            crate::protocols::schema::Sanitize {
                strip_ref_siblings: false,
                inject_empty_properties: false,
                strip_numeric_constraints: true,
            },
        );
        body["output_config"] = json!({"format": {"type": "json_schema", "schema": schema}});
    }
    if !options.contains_key("container")
        && let Some(container) = container(&request.messages)
    {
        body["container"] = Value::String(container);
    }
    Ok(body)
}

/// Ask Anthropic to drop a thinking block bound to another context rather
/// than reject the request, when `body` thinks adaptively (the default when
/// it names no thinking), the one mode that takes the binding. Whether it
/// asked.
fn drop_unbound(body: &mut Value) -> bool {
    let binding = json!({"prefix_mismatch_behavior": "drop_block"});
    match body.get_mut("thinking") {
        None => body["thinking"] = json!({"type": "adaptive", "block_binding": binding}),
        Some(Value::Object(thinking))
            if thinking.get("type").and_then(Value::as_str) == Some("adaptive") =>
        {
            thinking.entry("block_binding").or_insert(binding);
        }
        Some(_) => return false,
    }
    true
}

/// A request tool: strict, its schema in the subset constrained decoding
/// compiles, where it asks and the host implements it; asking for its input
/// as it is written when the host takes the field.
fn tool_json(tool: &crate::spec::Tool, quirks: Quirks) -> Value {
    let strict = tool.strict == Some(true) && quirks.strict_tool_schemas;
    let mut schema = tool.schema.clone();
    if strict {
        strict::sanitize(&mut schema);
    }
    let mut definition = json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": schema,
    });
    if strict {
        definition["strict"] = Value::Bool(true);
    }
    if quirks.tool_input_streaming == ToolInputStreaming::Eager {
        definition["eager_input_streaming"] = Value::Bool(true);
    }
    definition
}

/// The request's `tool_choice` before the options merge over it. A forced
/// choice a model rejects is left to the model; `parallel_tool_calls: false`
/// sets `disable_parallel_tool_use` where the dialect takes it.
fn tool_choice(dialect: Dialect, model: &str, request: &Request, has_tools: bool) -> Option<Value> {
    let forced_rejected = dialect == Dialect::Anthropic && rejects_forced_tool_choice(model);
    let mut choice = match &request.tool_choice {
        ToolChoice::Auto => None,
        ToolChoice::None => return Some(json!({"type": "none"})),
        ToolChoice::Required | ToolChoice::Tool(_) if forced_rejected => None,
        ToolChoice::Required => Some(json!({"type": "any"})),
        ToolChoice::Tool(name) => Some(json!({"type": "tool", "name": name})),
    };
    if request.parallel_tool_calls == Some(false)
        && has_tools
        && options::takes_parallel_off(dialect)
    {
        choice.get_or_insert_with(|| json!({"type": "auto"}))["disable_parallel_tool_use"] =
            Value::Bool(true);
    }
    choice
}

/// One turn of the conversation on the wire: tool results join the user
/// turn they lead.
enum Turn<'a> {
    System(String),
    User(Vec<&'a Message>),
    Assistant(&'a Assistant),
}

/// Split `messages` into the top-level `system` blocks and the turns.
///
/// On a model that takes mid-conversation system messages (`mid`), one in
/// a position Anthropic rejects (after an assistant turn, or before a user
/// turn) moves to the next valid slot: right after the next user turn that
/// ends the conversation or precedes an assistant turn. Hoisting it into
/// `system` instead would change the prompt prefix, missing the cache and,
/// on models that bind thinking to their context, turning every earlier
/// thinking block into an error. It is hoisted only when no slot exists.
fn split_system(messages: &[Message], mid: bool) -> (Vec<Value>, Vec<Turn<'_>>) {
    let mut turns: Vec<Turn<'_>> = Vec::new();
    for message in messages {
        match message {
            Message::System(system) => turns.push(Turn::System(system.text())),
            Message::User(_) | Message::Tool(_) => match turns.last_mut() {
                Some(Turn::User(run)) => run.push(message),
                _ => turns.push(Turn::User(vec![message])),
            },
            Message::Assistant(assistant) => turns.push(Turn::Assistant(assistant)),
        }
    }
    let mut system = Vec::new();
    let mut remaining = Vec::new();
    let mut deferred: Vec<String> = Vec::new();
    let lead = turns
        .iter()
        .take_while(|turn| matches!(turn, Turn::System(_)))
        .count();
    let valid: Vec<bool> = (0..turns.len())
        .map(|index| valid_system_message(&turns, index))
        .collect();
    let slots: Vec<bool> = (0..turns.len())
        .map(|index| system_slot(&turns, index))
        .collect();
    for (index, turn) in turns.into_iter().enumerate() {
        match turn {
            Turn::System(text) if text.is_empty() => {}
            // The system messages that lead the conversation are the prompt.
            Turn::System(text) if index < lead => system.push(text_block(&text)),
            Turn::System(text) if mid && valid[index] => remaining.push(Turn::System(text)),
            Turn::System(text) if mid && index > 0 && slots[index + 1..].contains(&true) => {
                deferred.push(text);
            }
            Turn::System(text) => system.push(text_block(&text)),
            other => {
                remaining.push(other);
                if !deferred.is_empty() && slots[index] {
                    remaining.push(Turn::System(deferred.join("\n\n")));
                    deferred.clear();
                }
            }
        }
    }
    (system, remaining)
}

/// Whether a system message may sit right after `turns[index]`: it is a
/// user turn, and the next turn that is not a system message is an
/// assistant turn or there is none.
fn system_slot(turns: &[Turn<'_>], index: usize) -> bool {
    matches!(turns.get(index), Some(Turn::User(_)))
        && turns
            .get(index + 1..)
            .into_iter()
            .flatten()
            .find(|turn| !matches!(turn, Turn::System(_)))
            .is_none_or(|turn| matches!(turn, Turn::Assistant(_)))
}

/// Whether the system message at `index` sits where Anthropic takes one:
/// after a user turn, or an assistant turn ending in a server tool's result,
/// and before an assistant turn or the end.
fn valid_system_message(turns: &[Turn<'_>], index: usize) -> bool {
    // A run of system messages shares one slot.
    let not_system = |turn: &&Turn<'_>| !matches!(turn, Turn::System(_));
    let follows = match turns
        .get(..index)
        .into_iter()
        .flatten()
        .rev()
        .find(not_system)
    {
        Some(Turn::User(_)) => true,
        // The `container` block goes to the request's top level.
        Some(Turn::Assistant(assistant)) => assistant
            .content
            .iter()
            .rev()
            .map(opaque_kind)
            .find(|kind| *kind != Some("container"))
            .flatten()
            .is_some_and(|kind| kind.ends_with("_tool_result")),
        Some(Turn::System(_)) | None => false,
    };
    follows
        && turns
            .get(index + 1..)
            .into_iter()
            .flatten()
            .find(not_system)
            .is_none_or(|turn| matches!(turn, Turn::Assistant(_)))
}

/// The provider type of an opaque part this protocol kept.
fn opaque_kind(part: &AssistantPart) -> Option<&str> {
    match part {
        AssistantPart::Opaque(opaque) => opaque.meta.get(KEY)?.at("/item/type")?.as_str(),
        _ => None,
    }
}

/// The id of the container the last turn holding one ran in. Anthropic
/// requires it on a request that answers a programmatic tool call, and it
/// keeps a code-execution session's state.
fn container(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .flat_map(|assistant| assistant.content.iter().rev())
        .find_map(|part| match part {
            AssistantPart::Opaque(opaque) if opaque_kind(part) == Some("container") => opaque
                .meta
                .get(KEY)?
                .at("/item/container/id")?
                .as_str()
                .map(str::to_owned),
            _ => None,
        })
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// `block` with the fields its part's state under [`KEY`] names, such as a
/// document's `title` or a `cache_control` marker.
fn with_meta(mut block: Value, meta: &Meta) -> Value {
    if let Some(Value::Object(fields)) = meta.get(KEY) {
        crate::json::overlay(&mut block, fields);
    }
    block
}

/// `turn` on the wire; `None` when no block is left to send.
fn turn_json(turn: &Turn<'_>) -> Result<Option<Value>, Error> {
    let (role, content) = match turn {
        Turn::System(text) => ("system", vec![text_block(text)]),
        Turn::User(run) => {
            // Anthropic requires a turn's tool results before its text.
            let mut results = Vec::new();
            let mut parts = Vec::new();
            for message in run {
                match message {
                    Message::Tool(tool) => {
                        for result in &tool.results {
                            results.push(result_json(result)?);
                        }
                    }
                    Message::User(user) => {
                        for part in &user.content {
                            parts.extend(user_part(part)?);
                        }
                    }
                    Message::System(_) | Message::Assistant(_) => {}
                }
            }
            results.append(&mut parts);
            ("user", results)
        }
        Turn::Assistant(assistant) => ("assistant", assistant_blocks(assistant)),
    };
    Ok((!content.is_empty()).then(|| json!({"role": role, "content": content})))
}

/// One user block; `None` for blank text, which Anthropic rejects.
fn user_part(part: &UserPart) -> Result<Option<Value>, Error> {
    Ok(Some(match part {
        UserPart::Text(text) if text.text.trim().is_empty() => return Ok(None),
        UserPart::Text(text) => with_meta(text_block(&text.text), &text.meta),
        UserPart::Media(media) => with_meta(media_block(media)?, &media.meta),
    }))
}

fn result_json(result: &ToolResult) -> Result<Value, Error> {
    let content = match &result.output {
        Output::Text(text) => vec![text_block(text)],
        Output::Json(value) => vec![text_block(&value.to_string())],
        Output::Content(parts) => parts
            .iter()
            .map(|part| match part {
                UserPart::Text(text) => Ok(text_block(&text.text)),
                UserPart::Media(media) => media_block(media),
            })
            .collect::<Result<_, _>>()?,
    };
    let mut block = json!({
        "type": "tool_result",
        "tool_use_id": result.id,
        "content": content,
    });
    if result.error {
        block["is_error"] = Value::Bool(true);
    }
    Ok(block)
}

fn unsendable(media: &Media) -> Error {
    Error::unsupported(format!(
        "the Messages API cannot carry this {} media",
        if media.media_type.is_empty() {
            "untyped"
        } else {
            &media.media_type
        }
    ))
}

/// An image or a document block for `media`.
fn media_block(media: &Media) -> Result<Value, Error> {
    let kind = media.media_type.as_str();
    if kind.starts_with("audio/") || kind.starts_with("video/") {
        return Err(unsendable(media));
    }
    Ok(if kind.starts_with("image/") {
        json!({"type": "image", "source": image_source(media).ok_or_else(|| unsendable(media))?})
    } else {
        json!({"type": "document", "source": document_source(media).ok_or_else(|| unsendable(media))?})
    })
}

/// An image's source: base64 data of a type Anthropic reads, a URL, or a
/// Files API id.
fn image_source(media: &Media) -> Option<Value> {
    Some(match &media.source {
        Source::Data { data } => {
            let kind = media.media_type.as_str();
            if !matches!(
                kind,
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            ) {
                return None;
            }
            json!({"type": "base64", "media_type": kind, "data": data})
        }
        Source::Url { url } => json!({"type": "url", "url": url}),
        Source::File { id } => json!({"type": "file", "file_id": id}),
    })
}

/// A document's source: a Files API id, a PDF as data or by URL, or any
/// other typed document as the text it holds.
fn document_source(media: &Media) -> Option<Value> {
    let kind = media.media_type.as_str();
    Some(match &media.source {
        Source::File { id } => json!({"type": "file", "file_id": id}),
        // Anthropic's URL source is for PDFs and names no type, so an
        // untyped URL is one.
        Source::Url { url } if kind.is_empty() || kind == "application/pdf" => {
            json!({"type": "url", "url": url})
        }
        Source::Url { .. } => return None,
        Source::Data { data } if kind == "application/pdf" => {
            json!({"type": "base64", "media_type": kind, "data": data})
        }
        Source::Data { .. } if kind.is_empty() => return None,
        Source::Data { data } => {
            let text = String::from_utf8(base64(data)?).ok()?;
            json!({"type": "text", "media_type": "text/plain", "data": text})
        }
    })
}

/// Standard base64 decoded, padding optional; `None` for anything else.
fn base64(text: &str) -> Option<Vec<u8>> {
    const fn digit(byte: u8) -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => (byte - b'A') as u32,
            b'a'..=b'z' => (byte - b'a') as u32 + 26,
            b'0'..=b'9' => (byte - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let bytes = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut word = 0;
        for (at, byte) in chunk.iter().enumerate() {
            word |= digit(*byte)? << (18 - 6 * at);
        }
        let word = word.to_be_bytes();
        out.extend_from_slice(&word[1..chunk.len()]);
    }
    Some(out)
}

/// An assistant turn's blocks: each part's provider block while it is
/// current, else the part rebuilt. A server tool's result goes only with
/// its use, and the reply's container goes as the request's `container`.
fn assistant_blocks(assistant: &Assistant) -> Vec<Value> {
    let mut uses: Vec<&str> = Vec::new();
    let mut blocks = Vec::new();
    for part in &assistant.content {
        if let AssistantPart::Opaque(opaque) = part {
            let item = opaque.meta.get(KEY).and_then(|state| state.get("item"));
            let Some(item) = item.filter(|_| replays(part, KEY)) else {
                continue;
            };
            let kind = item.str("type").unwrap_or_default();
            if kind == "container" {
                continue;
            }
            if kind.ends_with("_tool_result")
                && !item.str("tool_use_id").is_some_and(|id| uses.contains(&id))
            {
                continue;
            }
            if kind.ends_with("_tool_use")
                && let Some(id) = item.str("id")
            {
                uses.push(id);
            }
            blocks.push(sendable(item));
            continue;
        }
        let identity = match replay(part, KEY, identity) {
            Replay::Item(item) => {
                let mut item = sendable(item);
                if let AssistantPart::ToolCall(call) = part {
                    // The call takes the id its result answers.
                    item["id"] = call.id.as_str().into();
                }
                blocks.push(item);
                continue;
            }
            Replay::Identity(identity) => identity,
            Replay::Rebuild => Map::new(),
        };
        let block = match part {
            AssistantPart::Text(text) if text.text.trim().is_empty() => continue,
            AssistantPart::Text(text) => text_block(&text.text),
            // Thinking without its signature is text; a redacted block's
            // payload lived only in its item.
            AssistantPart::Reasoning(reasoning) => {
                if reasoning.redacted || reasoning.text.trim().is_empty() {
                    continue;
                }
                text_block(&reasoning.text)
            }
            AssistantPart::ToolCall(call) => {
                let mut block = json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.input,
                });
                if let Some(fields) = block.as_object_mut() {
                    fields.extend(identity);
                }
                block
            }
            AssistantPart::Media(_) => text_block(ASSISTANT_IMAGE_OMITTED),
            AssistantPart::ToolResult(_) | AssistantPart::Opaque(_) => continue,
        };
        blocks.push(block);
    }
    blocks
}

/// A streamed block as the input side takes it back. Input blocks are
/// validated strictly, so the core kinds keep only their input fields (a
/// gateway may add output-only ones such as `estimated_tokens`), and every
/// kind loses the fields a stream states as `null`.
fn sendable(item: &Value) -> Value {
    let keep: &[&str] = match item.str("type") {
        Some("thinking") => &["type", "thinking", "signature"],
        Some("redacted_thinking") => &["type", "data"],
        Some("text") => &["type", "text", "citations"],
        Some("tool_use") => &["type", "id", "name", "input", "caller"],
        _ => &[],
    };
    let Some(fields) = item.as_object() else {
        return item.clone();
    };
    Value::Object(
        fields
            .iter()
            .filter(|(key, value)| {
                !value.is_null() && (keep.is_empty() || keep.contains(&key.as_str()))
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}
