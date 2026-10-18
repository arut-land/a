//! A request as a Chat Completions body. Each assistant message is rebuilt
//! from its parts: text joined into `content`, reasoning under the field it
//! arrived in (or the host's), and each call from its item while the call
//! is unedited. Media a host or model cannot take becomes a short note the
//! model still reads, and a host without tools reads the conversation's
//! calls and results as text.

use std::borrow::Cow;

use serde_json::{Map, Value, json};

use super::decode::REASONING_TEXT_KEYS;
use super::merge::merge_fields;
use super::{KEY, LaterSystem, Quirks, Rewrite, Routing, Vendor, rules};
use crate::error::Error;
use crate::json::Lenient;
use crate::models::openai;
use crate::protocol::Target;
use crate::protocols::schema;
use crate::replay::{Replay, replay, replays};
use crate::spec::{
    Assistant, AssistantPart, Format, Media, Message, Output, Request, Source, ToolCall,
    ToolChoice, ToolResult, UserPart,
};

/// What replaces an image for a model that reads none.
pub(super) const USER_IMAGE_OMITTED: &str = "(image omitted: model does not support images)";
/// What replaces a tool-result image that has nowhere to go.
pub(super) const TOOL_IMAGE_OMITTED: &str = "(tool image omitted: model does not support images)";
/// What replaces a tool-result image that moved to a user message.
pub(super) const TOOL_IMAGE_ATTACHED: &str = "(see attached image)";
/// The text heading the user message that carries tool-result images.
pub(super) const TOOL_IMAGES_HEADING: &str = "Attached image(s) from tool result:";
/// What replaces media the host cannot take in its form.
pub(super) const IMAGE_UNSENDABLE: &str =
    "(image omitted: the provider cannot receive it in this form)";
pub(super) const AUDIO_UNSENDABLE: &str =
    "(audio omitted: the provider cannot receive it in this form)";
pub(super) const VIDEO_UNSENDABLE: &str =
    "(video omitted: the provider cannot receive it in this form)";
pub(super) const DOCUMENT_UNSENDABLE: &str =
    "(document omitted: the provider cannot receive it in this form)";
/// What answers a call whose result says nothing.
pub(super) const NO_TOOL_OUTPUT: &str = "(no tool output)";

/// The keys of a stale item a rebuilt part keeps: a custom call's kind and
/// Mistral's thinking part (`type`), or the field reasoning arrived in.
/// Signed data such as `reasoning_details` never survives an edit.
fn identity(item: &Value) -> Map<String, Value> {
    if let Some(kind @ ("custom" | "thinking")) = item.str("type") {
        return Map::from_iter([("type".to_owned(), Value::from(kind))]);
    }
    REASONING_TEXT_KEYS
        .iter()
        .find(|key| item.get(**key).is_some_and(Value::is_string))
        .map_or_default(|key| Map::from_iter([((*key).to_owned(), Value::String(String::new()))]))
}

/// What the encoder knows about the target.
struct Context<'a> {
    quirks: &'a Quirks,
    /// Whether the model reads user images.
    images: bool,
    reasoning_field: Option<&'static str>,
    /// The custom tools the request declares, whose calls are custom calls.
    custom: Vec<String>,
}

impl Context<'_> {
    /// Whether the host takes content parts at all.
    fn parts(&self) -> bool {
        !matches!(self.quirks.rewrite, Rewrite::DeepSeek | Rewrite::Mira)
    }

    /// Whether the host takes media beyond images.
    fn files(&self) -> bool {
        self.parts()
            && self.quirks.rewrite != Rewrite::Perplexity
            && !matches!(self.quirks.vendor, Vendor::Cohere | Vendor::Ollama)
    }

    /// Whether an image tool results carry is read there.
    fn result_images(&self) -> bool {
        self.images && self.quirks.image_tool_results
    }
}

fn text_part(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn data_url(media: &Media, data: &str) -> String {
    format!("data:{};base64,{data}", media.media_type)
}

/// The `image_url` part an image goes as, when the host takes it in its
/// form; a user image names a detail level, `auto`.
fn image_url(media: &Media, ctx: &Context<'_>, user: bool) -> Option<Value> {
    let url = match &media.source {
        Source::Data { data } if ctx.parts() => data_url(media, data),
        // Ollama reads an image as data and never fetches a URL.
        Source::Url { url } if ctx.parts() && ctx.quirks.vendor != Vendor::Ollama => url.clone(),
        Source::Data { .. } | Source::Url { .. } | Source::File { .. } => return None,
    };
    let mut part = json!({"url": url});
    if user {
        part["detail"] = json!("auto");
    }
    Some(json!({"type": "image_url", "image_url": part}))
}

/// The audio format a media type names.
fn audio_format(media_type: &str) -> &str {
    match media_type {
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        other => other.strip_prefix("audio/").unwrap_or(other),
    }
}

/// One user media part as the host takes it, or the note that replaces it.
fn media_part(media: &Media, ctx: &Context<'_>) -> Result<Value, &'static str> {
    let file = |file: Value| json!({"type": "file", "file": file});
    let kind = media.media_type.split('/').next().unwrap_or_default();
    match (kind, &media.source) {
        ("image", _) if !ctx.images => Err(USER_IMAGE_OMITTED),
        ("image", _) => image_url(media, ctx, true).ok_or(IMAGE_UNSENDABLE),
        ("audio", Source::Data { data }) if ctx.files() => Ok(json!({"type": "input_audio",
            "input_audio": {"data": data, "format": audio_format(&media.media_type)}})),
        ("audio", _) => Err(AUDIO_UNSENDABLE),
        ("video", Source::Data { data }) if ctx.files() && video(ctx) => {
            Ok(json!({"type": "video_url", "video_url": {"url": data_url(media, data)}}))
        }
        ("video", Source::Url { url }) if ctx.files() && video(ctx) => {
            Ok(json!({"type": "video_url", "video_url": {"url": url}}))
        }
        ("video", _) => Err(VIDEO_UNSENDABLE),
        (_, Source::File { id }) if ctx.files() && ctx.quirks.file_ids => {
            Ok(file(json!({ "file_id": id })))
        }
        _ if media.media_type != "application/pdf" => Err(DOCUMENT_UNSENDABLE),
        (_, Source::Data { data }) if ctx.files() => Ok(file(json!({
            "file_data": data_url(media, data),
            "filename": "document.pdf",
        }))),
        // OpenRouter and Mistral fetch a PDF a URL names.
        (_, Source::Url { url })
            if matches!(ctx.quirks.rewrite, Rewrite::OpenRouter | Rewrite::Mistral) =>
        {
            Ok(file(json!({"file_data": url, "filename": "document.pdf"})))
        }
        _ => Err(DOCUMENT_UNSENDABLE),
    }
}

/// Whether the host takes video: not `OpenAI`, Azure or Mistral.
fn video(ctx: &Context<'_>) -> bool {
    ctx.quirks.rewrite != Rewrite::Mistral
        && !matches!(ctx.quirks.vendor, Vendor::OpenAi | Vendor::Azure)
}

/// `note` after `parts`, unless the last part already says it.
fn note(parts: &mut Vec<Value>, note: &str) {
    if parts.last().and_then(|part| part.str("text")) != Some(note) {
        parts.push(text_part(note));
    }
}

/// A user message's parts: blank text says nothing, and several hosts
/// reject it.
fn user_parts(content: &[UserPart], ctx: &Context<'_>) -> Vec<Value> {
    let mut parts = Vec::new();
    for part in content {
        match part {
            UserPart::Text(text) if text.text.trim().is_empty() => {}
            UserPart::Text(text) => parts.push(text_part(&text.text)),
            UserPart::Media(media) => match media_part(media, ctx) {
                Ok(part) => parts.push(part),
                Err(placeholder) => note(&mut parts, placeholder),
            },
        }
    }
    parts
}

/// `parts` as a user message, joined to the user message before it: a lone
/// text part as a plain string.
fn push_user(messages: &mut Vec<Value>, mut parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(previous) = messages
        .last_mut()
        .filter(|m| m.str("role") == Some("user"))
    {
        let mut joined = match previous.get_mut("content").map(Value::take) {
            Some(Value::String(text)) => vec![text_part(&text)],
            Some(Value::Array(earlier)) => earlier,
            _ => Vec::new(),
        };
        joined.append(&mut parts);
        parts = joined;
        messages.pop();
    }
    let content = match parts.as_slice() {
        [part] if part.str("type") == Some("text") => part.get("text").cloned().unwrap_or_default(),
        _ => Value::Array(parts),
    };
    messages.push(json!({"role": "user", "content": content}));
}

/// A result's parts: text, and the images the `tool` message can carry.
/// One the model reads only from a user goes to `attached`, leaving a note.
fn result_parts(result: &ToolResult, ctx: &Context<'_>, attached: &mut Vec<Value>) -> Vec<Value> {
    let mut parts = Vec::new();
    match &result.output {
        Output::Text(text) => parts.push(text_part(text)),
        Output::Json(value) => parts.push(text_part(&value.to_string())),
        Output::Content(content) => {
            for part in content {
                match part {
                    UserPart::Text(text) => parts.push(text_part(&text.text)),
                    UserPart::Media(media) if media.media_type.starts_with("image/") => {
                        if let Some(image) =
                            image_url(media, ctx, false).filter(|_| ctx.result_images())
                        {
                            parts.push(image);
                        } else if let Some(image) =
                            image_url(media, ctx, true).filter(|_| ctx.images)
                        {
                            attached.push(image);
                            note(&mut parts, TOOL_IMAGE_ATTACHED);
                        } else {
                            note(&mut parts, TOOL_IMAGE_OMITTED);
                        }
                    }
                    UserPart::Media(media) => {
                        note(
                            &mut parts,
                            media_part(media, ctx).err().unwrap_or(DOCUMENT_UNSENDABLE),
                        );
                    }
                }
            }
        }
    }
    parts
}

/// The `tool` message that answers a call: its text joined into one
/// string, or its parts when the host asks for them or one is an image. A
/// result that says nothing says so.
fn tool_message(result: &ToolResult, ctx: &Context<'_>, attached: &mut Vec<Value>) -> Value {
    let mut parts = result_parts(result, ctx, attached);
    let image = |part: &Value| part.str("type") == Some("image_url");
    let blank = parts
        .iter()
        .all(|part| !image(part) && part.str("text").is_none_or(|text| text.trim().is_empty()));
    if blank {
        let text = if result.error {
            format!("[tool error] {NO_TOOL_OUTPUT}")
        } else {
            NO_TOOL_OUTPUT.to_owned()
        };
        parts = vec![text_part(&text)];
    }
    let content = if ctx.quirks.tool_result_parts || parts.iter().any(image) {
        Value::Array(parts)
    } else {
        let texts: Vec<&str> = parts.iter().filter_map(|part| part.str("text")).collect();
        Value::String(texts.join("\n"))
    };
    json!({"role": "tool", "tool_call_id": result.id, "content": content})
}

/// A result as the text a host without tools reads.
fn result_text(result: &ToolResult) -> String {
    let text = match &result.output {
        Output::Text(text) => text.clone(),
        Output::Json(value) => value.to_string(),
        Output::Content(parts) => parts
            .iter()
            .map(|part| match part {
                UserPart::Text(text) => text.text.as_str(),
                UserPart::Media(_) => TOOL_IMAGE_OMITTED,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let kind = if result.error { "error" } else { "result" };
    format!("[tool {} {kind}] {text}", result.name)
}

/// The conversation with every later system message joined into the
/// leading one, for a host that takes system text only there.
fn leading_system(messages: &[Message]) -> Cow<'_, [Message]> {
    let later = messages
        .iter()
        .skip_while(|message| matches!(message, Message::System(_)))
        .any(|message| matches!(message, Message::System(_)));
    if !later {
        return Cow::Borrowed(messages);
    }
    let prompt: Vec<String> = messages
        .iter()
        .filter_map(|message| match message {
            Message::System(system) => Some(system.text()),
            _ => None,
        })
        .filter(|text| !text.trim().is_empty())
        .collect();
    let mut joined = Vec::with_capacity(messages.len());
    if !prompt.is_empty() {
        joined.push(Message::system(prompt.join("\n\n")));
    }
    joined.extend(
        messages
            .iter()
            .filter(|message| !matches!(message, Message::System(_)))
            .cloned(),
    );
    Cow::Owned(joined)
}

fn messages(history: &[Message], ctx: &Context<'_>) -> Result<Vec<Value>, Error> {
    let history = match ctx.quirks.later_system {
        LaterSystem::Leading => leading_system(history),
        LaterSystem::InPlace => Cow::Borrowed(history),
    };
    let mut messages = Vec::new();
    for message in history.iter() {
        match message {
            Message::System(system) => {
                let text = system.text();
                if !text.trim().is_empty() {
                    messages.push(json!({"role": "system", "content": [text_part(&text)]}));
                }
            }
            Message::User(user) => push_user(&mut messages, user_parts(&user.content, ctx)),
            Message::Tool(tool) if !ctx.quirks.supports_tools => {
                let parts = tool
                    .results
                    .iter()
                    .map(|result| text_part(&result_text(result)))
                    .collect();
                push_user(&mut messages, parts);
            }
            Message::Tool(tool) => {
                let mut attached = Vec::new();
                for result in &tool.results {
                    messages.push(tool_message(result, ctx, &mut attached));
                }
                if !attached.is_empty() {
                    attached.insert(0, text_part(TOOL_IMAGES_HEADING));
                    push_user(&mut messages, attached);
                }
            }
            Message::Assistant(assistant) => {
                if let Some(message) = assistant_message(assistant, ctx) {
                    messages.push(message);
                }
            }
        }
    }
    if messages.is_empty() {
        return Err(Error::unsupported(
            "a Chat Completions request needs at least one message the host takes",
        ));
    }
    Ok(messages)
}

/// One assistant message rebuilt from its parts: text joined into a
/// `content` string, or a part array when a part is a content part
/// (Mistral's thinking); reasoning under the field it arrived in, or the
/// host's; an item's other fields beside it; and each call with its name
/// and arguments. A message with neither content nor calls is not sent.
fn assistant_message(assistant: &Assistant, ctx: &Context<'_>) -> Option<Value> {
    let (mut text, mut parts, mut has_parts) = (String::new(), Vec::new(), false);
    let mut reasoning: Vec<(String, String)> = Vec::new();
    let (mut fields, mut calls) = (Map::new(), Vec::new());
    for part in &assistant.content {
        let sent = replay(part, KEY, identity);
        match part {
            AssistantPart::Text(block) => {
                text.push_str(&block.text);
                parts.push(text_part(&block.text));
                // A text item is message fields: an answer's audio.
                if let Replay::Item(Value::Object(item)) = sent {
                    fields.extend(item.clone());
                }
            }
            AssistantPart::Reasoning(block) => {
                match reasoning_back(&block.text, sent, ctx, &mut fields) {
                    Back::Part(part) => {
                        has_parts = true;
                        parts.push(part);
                    }
                    Back::Field(field) => {
                        match reasoning.iter_mut().find(|(name, _)| *name == field) {
                            Some((_, joined)) => {
                                joined.push('\n');
                                joined.push_str(&block.text);
                            }
                            None => reasoning.push((field, block.text.clone())),
                        }
                    }
                    Back::Nowhere => {}
                }
            }
            // A host without tools reads a call as text.
            AssistantPart::ToolCall(call) if !ctx.quirks.supports_tools => {
                let said = format!(
                    "[called tool {} with {}]",
                    call.name,
                    Value::Object(call.input.clone())
                );
                text.push_str(&said);
                parts.push(text_part(&said));
            }
            AssistantPart::ToolCall(call) => calls.push(call_item(call, sent, &ctx.custom)),
            // An opaque item is a content part; one with no `type` is not.
            AssistantPart::Opaque(opaque) if replays(part, KEY) => {
                if let Some(item) = opaque
                    .meta
                    .get(KEY)
                    .and_then(|state| state.get("item"))
                    .filter(|item| item.get("type").is_some())
                {
                    has_parts = true;
                    parts.push(item.clone());
                }
            }
            AssistantPart::Opaque(_) | AssistantPart::Media(_) | AssistantPart::ToolResult(_) => {}
        }
    }
    let mut message = Map::from_iter([("role".to_owned(), Value::from("assistant"))]);
    if has_parts {
        message.insert("content".to_owned(), Value::Array(parts));
    } else if !text.is_empty() {
        message.insert("content".to_owned(), Value::String(text));
    }
    for (field, text) in reasoning {
        message.insert(field, Value::String(text));
    }
    message.extend(fields);
    if !calls.is_empty() {
        message.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    let has_content = message.contains_key("audio")
        || message.contains_key("tool_calls")
        || match message.get("content") {
            Some(Value::String(text)) => !text.is_empty(),
            Some(Value::Array(parts)) => !parts.is_empty(),
            _ => false,
        };
    if let Some(field) = ctx.reasoning_field {
        message
            .entry(field)
            .or_insert_with(|| Value::String(String::new()));
    }
    has_content.then_some(Value::Object(message))
}

/// Where a reasoning part goes back.
enum Back {
    /// As a content part (Mistral's thinking).
    Part(Value),
    /// Under a message field.
    Field(String),
    /// Nowhere: the host takes no rebuilt reasoning.
    Nowhere,
}

/// Where reasoning `text` goes back, from what replay hands the encoder:
/// its current item's part or field (its other fields, such as
/// `reasoning_details`, joining `fields`), an edited one's, or the host's
/// field.
fn reasoning_back(
    text: &str,
    sent: Replay<'_>,
    ctx: &Context<'_>,
    fields: &mut Map<String, Value>,
) -> Back {
    let field = match sent {
        Replay::Item(item) if item.get("type").is_some() => return Back::Part(item.clone()),
        Replay::Item(item) => {
            let mut field = None;
            for (key, value) in item.as_object().into_iter().flatten() {
                if value.is_string() {
                    field.get_or_insert_with(|| key.clone());
                } else {
                    merge_fields(fields, &Map::from_iter([(key.clone(), value.clone())]));
                }
            }
            field
        }
        Replay::Identity(identity)
            if identity.get("type").and_then(Value::as_str) == Some("thinking") =>
        {
            return Back::Part(
                json!({"type": "thinking", "thinking": [{"type": "text", "text": text}]}),
            );
        }
        Replay::Identity(identity) => identity.keys().next().cloned(),
        Replay::Rebuild => ctx.reasoning_field.map(str::to_owned),
    };
    field.map_or(Back::Nowhere, Back::Field)
}

/// `call` as the item the host takes: its current item, an edited one's
/// kind, or nothing. A call is custom when its item says so, or when it is
/// rebuilt and the request declares a custom tool by its name; its input is
/// then its arguments' `input`.
fn call_item(call: &ToolCall, sent: Replay<'_>, custom: &[String]) -> Value {
    let mut item = match sent {
        Replay::Item(Value::Object(item)) => item.clone(),
        Replay::Identity(identity) => identity,
        Replay::Item(_) | Replay::Rebuild => Map::new(),
    };
    let custom = if let Some(kind) = item.get("type").and_then(Value::as_str) {
        kind == "custom"
    } else {
        let declared = custom.contains(&call.name);
        let kind = if declared { "custom" } else { "function" };
        item.insert("type".to_owned(), Value::from(kind));
        declared
    };
    item.insert("id".to_owned(), Value::String(call.id.clone()));
    let (slot, key, value) = if custom {
        let input = match call.input.get("input") {
            Some(Value::String(input)) => input.clone(),
            Some(input) => input.to_string(),
            None => String::new(),
        };
        ("custom", "input", input)
    } else {
        let arguments = Value::Object(call.input.clone()).to_string();
        ("function", "arguments", arguments)
    };
    let slot = item
        .entry(slot)
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    if let Value::Object(fields) = slot {
        fields.insert("name".to_owned(), Value::from(call.name.as_str()));
        fields.insert(key.to_owned(), Value::String(value));
    }
    Value::Object(item)
}

fn function_tool(tool: &crate::spec::Tool, strict_by_default: bool) -> Value {
    let strict = tool.strict.unwrap_or(strict_by_default);
    let mut parameters = tool.schema.clone();
    if strict {
        schema::openai(&mut parameters);
    }
    let mut function = json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": parameters,
    });
    if strict {
        function["strict"] = Value::Bool(true);
    }
    json!({"type": "function", "function": function})
}

fn tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool(name) => json!({"type": "function", "function": {"name": name}}),
    }
}

/// The `response_format` a JSON format takes: a schema strict, or JSON
/// mode.
fn response_format(format: &Format) -> Option<Value> {
    let Format::Json {
        schema: json_schema,
        name,
        ..
    } = format
    else {
        return None;
    };
    let Some(json_schema) = json_schema else {
        return Some(json!({"type": "json_object"}));
    };
    let mut json_schema = json_schema.clone();
    schema::openai(&mut json_schema);
    let name = name
        .as_deref()
        .or_else(|| json_schema.str("title"))
        .unwrap_or("response_schema")
        .to_owned();
    Some(json!({"type": "json_schema",
        "json_schema": {"name": name, "strict": true, "schema": json_schema}}))
}

/// The request body for `request` to `target`.
pub(super) fn body(
    quirks: &Quirks,
    target: &Target<'_>,
    request: &Request,
) -> Result<Value, Error> {
    let model = target.model;
    let mut options = request
        .options
        .get(KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // The request's own tools, function or native (Groq's
    // `browser_search`), join the typed ones.
    let passthrough = match options.remove("tools") {
        Some(Value::Array(tools)) => tools,
        _ => Vec::new(),
    };
    let ctx = Context {
        quirks,
        images: rules::reads_images(quirks.vendor, model),
        reasoning_field: rules::reasoning_field(quirks, &target.provider.base_url, model),
        custom: passthrough
            .iter()
            .filter(|tool| tool.str("type") == Some("custom"))
            .filter_map(|tool| tool.at("/custom/name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect(),
    };
    let messages = messages(&request.messages, &ctx)?;

    let mut tools: Vec<Value> = Vec::new();
    if quirks.supports_tools {
        tools.extend(
            request
                .tools
                .iter()
                .map(|tool| function_tool(tool, quirks.strict_tools)),
        );
    }
    tools.extend(passthrough);
    let offers_tools = quirks.supports_tools && !tools.is_empty();
    if offers_tools
        && matches!(quirks.vendor, Vendor::OpenAi | Vendor::Azure)
        && !openai::chat_calls_tools(model)
    {
        return Err(Error::unsupported(
            "the model cannot call tools through Chat Completions; use the Responses API",
        ));
    }

    let mut body = Map::new();
    if quirks.routing == Routing::Path {
        body.insert("model".into(), json!(model));
    }
    // A format waits for the first tool result unless the host takes both
    // at once, so the model can still call a tool.
    let answered = messages
        .iter()
        .any(|message| message.str("role") == Some("tool"));
    if let Some(format) = request
        .format
        .as_ref()
        .filter(|_| quirks.supports_response_format)
        .filter(|_| quirks.response_format_with_tools || tools.is_empty() || answered)
        .and_then(response_format)
    {
        body.insert("response_format".into(), format);
    }
    body.insert("messages".into(), Value::Array(messages));
    if offers_tools && request.tool_choice != ToolChoice::Auto {
        body.insert("tool_choice".into(), tool_choice(&request.tool_choice));
    }
    let has_tools = !tools.is_empty();
    if has_tools {
        body.insert("tools".into(), Value::Array(tools));
    }
    body.extend(rules::generation(
        quirks,
        model,
        &request.generation,
        request.parallel_tool_calls,
        has_tools,
    ));
    if let (Some(key), Vendor::OpenAi) = (&request.cache_key, quirks.vendor) {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    body.insert("stream".into(), json!(true));
    let mut body = Value::Object(body);
    crate::protocol::merge(&mut body, &options);
    if let Some(fields) = body.as_object_mut() {
        super::rewrite::finish(quirks, fields)?;
    }
    Ok(body)
}
