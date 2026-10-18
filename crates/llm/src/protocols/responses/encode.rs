//! A request as a Responses body. Turns this protocol produced replay their
//! provider items verbatim, an edited part keeps its item's identity, and
//! everything else is rebuilt from the shared fields.

use serde_json::{Map, Value, json};

use super::{Instructions, KEY, Responses};
use crate::error::Error;
use crate::json::Lenient;
use crate::models::openai;
use crate::protocol::Target;
use crate::protocols::schema;
use crate::replay::{Replay, replay, replays};
use crate::spec::{
    AssistantPart, Format, Media, Message, Output, Request, Source, ToolChoice, ToolResult,
    UserPart,
};

/// The keys of a stale item a rebuilt part keeps.
pub(crate) fn identity(item: &Value) -> Map<String, Value> {
    let keys: &[&str] = match item.str("type") {
        Some("message") => &["type", "id", "phase"],
        Some("reasoning") => &["type", "id", "encrypted_content"],
        Some("function_call" | "custom_tool_call") => &["type", "id"],
        _ => &[],
    };
    keys.iter()
        .filter_map(|key| Some(((*key).to_owned(), item.get(*key)?.clone())))
        .collect()
}

fn unsendable(part: &str) -> Error {
    Error::unsupported(format!("the Responses API cannot carry this {part}"))
}

/// The `input_image` part `media` goes as.
fn image_part(media: &Media) -> Value {
    let (key, source) = match &media.source {
        Source::Data { data } => (
            "image_url",
            format!("data:{};base64,{data}", media.media_type),
        ),
        Source::Url { url } => ("image_url", url.clone()),
        Source::File { id } => ("file_id", id.clone()),
    };
    json!({"type": "input_image", "detail": "auto", key: source})
}

/// The content part `media` goes as: an image, or an `input_file`.
fn media_part(media: &Media) -> Result<Value, Error> {
    if media.media_type.starts_with("image/") {
        return Ok(image_part(media));
    }
    Ok(match &media.source {
        Source::File { id } => json!({"type": "input_file", "file_id": id}),
        // `input_file` reads the type of a file it fetches itself.
        Source::Url { url } => json!({"type": "input_file", "file_url": url}),
        Source::Data { data } if media.media_type == "application/pdf" => json!({
            "type": "input_file",
            "file_data": format!("data:application/pdf;base64,{data}"),
            "filename": "document.pdf",
        }),
        Source::Data { .. } => return Err(unsendable(&format!("{} file", media.media_type))),
    })
}

/// A tool result's `output`: one text as a string, anything else as its
/// ordered parts.
fn result_output(output: &Output) -> Result<Value, Error> {
    Ok(match output {
        Output::Text(text) => Value::String(text.clone()),
        Output::Json(value) => Value::String(value.to_string()),
        Output::Content(parts) => {
            let mut parts = parts
                .iter()
                .map(|part| match part {
                    UserPart::Text(text) => Ok(json!({"type": "input_text", "text": text.text})),
                    UserPart::Media(media) => media_part(media),
                })
                .collect::<Result<Vec<_>, _>>()?;
            match parts.as_mut_slice() {
                [part] if part.str("type") == Some("input_text") => {
                    part.get_mut("text").map_or_default(Value::take)
                }
                _ => Value::Array(parts),
            }
        }
    })
}

fn result_item(result: &ToolResult, custom: bool) -> Result<Value, Error> {
    // A function output has no error field: a failed result says so in its
    // text.
    let output = result_output(&result.output)?;
    Ok(if custom {
        json!({"type": "custom_tool_call_output", "call_id": result.id, "output": output})
    } else {
        json!({"type": "function_call_output", "call_id": result.id, "output": output, "status": "completed"})
    })
}

/// The input items of `messages`. A `stateless` request resolves no stored
/// item, so reasoning without its ciphertext is left out, and the part after
/// it is rebuilt with no item id, as another model's turn is.
fn input(messages: &[Message], stateless: bool) -> Result<Vec<Value>, Error> {
    // Whether each call id the request sends is a custom call.
    let mut custom = std::collections::HashMap::<String, bool>::new();
    let mut items = Vec::new();
    for (position, message) in messages.iter().enumerate() {
        match message {
            Message::System(system) => items.push(json!({
                "type": "message",
                "role": "system",
                "content": [{"type": "input_text", "text": system.text()}],
            })),
            Message::User(user) => {
                for part in &user.content {
                    let part = match part {
                        // Blank text says nothing, and some backends reject it.
                        UserPart::Text(text) if text.text.trim().is_empty() => continue,
                        UserPart::Text(text) => json!({"type": "input_text", "text": text.text}),
                        UserPart::Media(media) => media_part(media)?,
                    };
                    items.push(json!({"type": "message", "role": "user", "content": [part]}));
                }
            }
            Message::Tool(tool) => {
                for result in &tool.results {
                    let kind = custom.get(&result.id).copied().unwrap_or(false);
                    items.push(result_item(result, kind)?);
                }
            }
            Message::Assistant(assistant) => {
                assistant_items(
                    &assistant.content,
                    position,
                    stateless,
                    &mut custom,
                    &mut items,
                )?;
            }
        }
    }
    Ok(items)
}

fn assistant_items(
    content: &[AssistantPart],
    position: usize,
    stateless: bool,
    custom: &mut std::collections::HashMap<String, bool>,
    items: &mut Vec<Value>,
) -> Result<(), Error> {
    let mut texts = 0usize;
    let mut unpaired = false;
    for part in content {
        let mut sent = replay(part, KEY, identity);
        if unpaired && !matches!(part, AssistantPart::Reasoning(_)) {
            unpaired = false;
            sent = Replay::Rebuild;
        }
        let ciphertext = |item: &Map<String, Value>| {
            item.get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|cipher| !cipher.is_empty())
        };
        let unresolvable = stateless
            && matches!(part, AssistantPart::Reasoning(_))
            && match &sent {
                Replay::Item(item) => !item.as_object().is_some_and(ciphertext),
                Replay::Identity(identity) => !ciphertext(identity),
                Replay::Rebuild => false,
            };
        if unresolvable {
            unpaired = true;
            continue;
        }
        let identity = match sent {
            Replay::Item(item) => {
                let mut item = item.clone();
                // A streamed item states unset fields as `null` (a reasoning
                // item's `status`), which the input side rejects as unknown.
                if let Some(fields) = item.as_object_mut() {
                    fields.retain(|_, value| !value.is_null());
                }
                if let AssistantPart::ToolCall(call) = part {
                    // The call takes the id its result answers.
                    item["call_id"] = Value::String(call.id.clone());
                    custom.insert(
                        call.id.clone(),
                        item.str("type") == Some("custom_tool_call"),
                    );
                }
                items.push(item);
                continue;
            }
            Replay::Identity(identity) => identity,
            Replay::Rebuild => Map::new(),
        };
        if let Some(item) = rebuilt(part, &identity, position, &mut texts, custom)? {
            items.push(item);
        }
    }
    Ok(())
}

/// A part rebuilt from its shared fields, keeping `identity` of its stale
/// item. `None` for a part that cannot go without its item.
fn rebuilt(
    part: &AssistantPart,
    identity: &Map<String, Value>,
    position: usize,
    texts: &mut usize,
    custom: &mut std::collections::HashMap<String, bool>,
) -> Result<Option<Value>, Error> {
    let id = |prefix: &str| {
        identity
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.starts_with(prefix) && id.len() <= 64)
            .map(str::to_owned)
    };
    let item = match part {
        AssistantPart::Text(text) => {
            let synthetic = match *texts {
                0 => format!("msg_a_{position}"),
                n => format!("msg_a_{position}_{n}"),
            };
            *texts += 1;
            let mut item = json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text.text, "annotations": []}],
                "status": "completed",
                "id": id("").unwrap_or(synthetic),
            });
            if let Some(phase) = identity.get("phase").filter(|phase| phase.is_string()) {
                item["phase"] = phase.clone();
            }
            item
        }
        AssistantPart::ToolCall(call) => {
            let arguments = Value::Object(call.input.clone()).to_string();
            let kind = identity.get("type").and_then(Value::as_str);
            let mut item = if kind == Some("custom_tool_call") {
                custom.insert(call.id.clone(), true);
                let input = call.input.get("input").and_then(Value::as_str);
                json!({
                    "type": "custom_tool_call",
                    "call_id": call.id,
                    "name": call.name,
                    "input": input.map_or(arguments, str::to_owned),
                })
            } else {
                custom.insert(call.id.clone(), false);
                json!({"type": "function_call", "call_id": call.id, "name": call.name, "arguments": arguments})
            };
            let prefix = if kind == Some("custom_tool_call") {
                "ctc_"
            } else {
                "fc_"
            };
            if let Some(id) = id(prefix) {
                item["id"] = Value::String(id);
            }
            item
        }
        // An edited reasoning part keeps its item's id and ciphertext,
        // so the items after it stay paired.
        AssistantPart::Reasoning(reasoning) => {
            let Some(rs) = id("") else {
                return Ok(None);
            };
            let summary: Vec<Value> = (!reasoning.text.is_empty())
                .then(|| json!({"type": "summary_text", "text": reasoning.text}))
                .into_iter()
                .collect();
            let mut item = json!({"type": "reasoning", "id": rs, "summary": summary});
            if let Some(ciphertext) = identity.get("encrypted_content") {
                item["encrypted_content"] = ciphertext.clone();
            }
            item
        }
        AssistantPart::Opaque(opaque) if replays(part, KEY) => {
            match opaque.meta.get(KEY).and_then(|state| state.get("item")) {
                Some(item) => item.clone(),
                None => return Ok(None),
            }
        }
        AssistantPart::Opaque(_) | AssistantPart::ToolResult(_) => return Ok(None),
        AssistantPart::Media(_) => return Err(unsendable("assistant image")),
    };
    Ok(Some(item))
}

fn tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool(name) => json!({"type": "function", "name": name}),
    }
}

/// The request body for `request` to `target`.
pub(crate) fn body(
    responses: &Responses,
    target: &Target<'_>,
    request: &Request,
) -> Result<Value, Error> {
    let model = target.model;
    let options = request
        .options
        .get(KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // Stateless unless the caller keeps the conversation on the provider:
    // every turn is sent whole, and reasoning replays from its ciphertext.
    let stateless = options.get("store").and_then(Value::as_bool) != Some(true)
        && !options.contains_key("previous_response_id")
        && !options.contains_key("conversation");
    let mut items = input(&request.messages, stateless)?;

    let instructions = lift_instructions(&mut items, responses.quirks.instructions)?;
    let mut tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| function_tool(tool, responses.quirks.strict_tools))
        .collect();
    if let Some(Value::Array(hosted)) = options.get("tools") {
        tools.extend(hosted.iter().cloned());
    }

    let generation = &request.generation;
    let effort = generation.reasoning;
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("input".into(), Value::Array(items));
    body.insert("stream".into(), json!(true));
    if !instructions.is_empty() {
        body.insert("instructions".into(), json!(instructions));
    }
    if let Some(max) = generation.max_output_tokens {
        body.insert("max_output_tokens".into(), json!(max.get()));
    }
    // Reasoning models reject sampling while they reason: dropped rather
    // than failing a request that would otherwise succeed.
    if openai::sampling(model).takes(effort) {
        if let Some(temperature) = generation.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if let Some(top_p) = generation.top_p {
            body.insert("top_p".into(), json!(top_p));
        }
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
        if request.tool_choice != ToolChoice::Auto {
            body.insert("tool_choice".into(), tool_choice(&request.tool_choice));
        }
        if let Some(parallel) = request.parallel_tool_calls {
            body.insert("parallel_tool_calls".into(), json!(parallel));
        }
    }
    if let Some(text) = request.format.as_ref().and_then(text_format) {
        body.insert("text".into(), text);
    }
    if let Some(reasoning) = reasoning(model, effort) {
        body.insert("reasoning".into(), reasoning);
    }
    if stateless {
        body.insert("store".into(), json!(false));
        if openai::reasons(model) != Some(false) {
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }
    if let Some(key) = &request.cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    // The caller's typed options last, but hosted tools went with the rest.
    for (key, value) in options {
        if key != "tools" {
            body.insert(key, value);
        }
    }
    Ok(Value::Object(body))
}

/// The system instructions `items` lifts out of `input` by `placement`.
fn lift_instructions(items: &mut Vec<Value>, placement: Instructions) -> Result<String, Error> {
    let system = |item: &Value| {
        (item.str("role") == Some("system")).then(|| {
            item.at("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
    };
    let before = items.len();
    let mut lifted = Vec::new();
    match placement {
        // The leading run of system messages, unless it is the whole
        // request, which then keeps them in `input` so it is not empty.
        Instructions::Leading => {
            let leading = items
                .iter()
                .take_while(|item| system(item).is_some())
                .count();
            if leading < items.len() {
                lifted.extend(items.drain(..leading).filter_map(|item| system(&item)));
            }
        }
        Instructions::All => items.retain(|item| match system(item) {
            Some(text) => {
                lifted.push(text);
                false
            }
            None => true,
        }),
        Instructions::Input => {}
    }
    if items.is_empty() {
        return Err(Error::unsupported(if items.len() < before {
            "a Responses request needs at least one non-system input item"
        } else {
            "a Responses request needs at least one input item"
        }));
    }
    let lifted: Vec<&str> = lifted
        .iter()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .collect();
    Ok(lifted.join("\n\n"))
}

fn function_tool(tool: &crate::spec::Tool, strict_by_default: bool) -> Value {
    let strict = tool.strict.unwrap_or(strict_by_default);
    let mut parameters = tool.schema.clone();
    if strict {
        schema::openai(&mut parameters);
    }
    let mut definition = json!({
        "type": "function",
        "name": tool.name,
        "parameters": parameters,
        // An omitted `strict` means "attempt strict mode", so `false` must
        // reach the wire.
        "strict": strict,
    });
    if !tool.description.is_empty() {
        definition["description"] = Value::String(tool.description.clone());
    }
    definition
}

/// The `text` field a structured output takes.
fn text_format(format: &Format) -> Option<Value> {
    let Format::Json {
        schema: json_schema,
        name,
        description,
    } = format
    else {
        return None;
    };
    let Some(json_schema) = json_schema else {
        return Some(json!({"format": {"type": "json_object"}}));
    };
    let mut json_schema = json_schema.clone();
    schema::openai(&mut json_schema);
    let mut format = json!({
        "type": "json_schema",
        "name": name.as_deref().unwrap_or("response_schema"),
        "schema": json_schema,
        "strict": true,
    });
    if let Some(description) = description {
        format["description"] = json!(description);
    }
    Some(json!({ "format": format }))
}

/// The `reasoning` field: the effort asked for, with summaries so reasoning
/// streams. A model that does not reason takes none, and one that cannot
/// turn reasoning off takes no effort `none`.
fn reasoning(model: &str, effort: Option<crate::Effort>) -> Option<Value> {
    use crate::Effort;
    match (effort, openai::reasons(model)) {
        (Some(Effort::None), _) if !openai::responses_can_disable(model) => None,
        (_, Some(false)) | (None, None) => None,
        (Some(Effort::None), _) => Some(json!({"effort": "none"})),
        (Some(effort), _) => Some(json!({"effort": effort.as_str(), "summary": "auto"})),
        (None, Some(true)) => Some(json!({"summary": "auto"})),
    }
}
