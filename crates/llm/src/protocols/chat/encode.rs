//! A request as a Chat Completions body. Each assistant message is rebuilt
//! from its parts: text joined into `content`, reasoning under the field it
//! arrived in (or the host's), and each call from its item while the call
//! is unedited. Media a host or model cannot take becomes a short note the
//! model still reads, and a host without tools reads the conversation's
//! calls and results as text.

use serde_json::{Map, Value, json};

use super::merge::merge_fields;
use super::{
    KEY, LaterSystem, PartKind, Quirks, ReasoningField, Routing, Vendor as V, rewrite, rules,
};
use crate::conversation::{self, Fold, History};
use crate::error::Error;
use crate::json::{Lenient, obj};
use crate::models::openai;
use crate::protocol::Target;
use crate::protocols::schema;
use crate::replay::{Replay, replay, replays};
use crate::spec::{
    Assistant, AssistantPart, Format, Media, Message, Output, Request, Source, Tool, ToolCall,
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

/// Who a message is from.
#[derive(Clone, Copy)]
pub(super) enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    /// Whether `message` is from this role.
    fn sent(self, message: &Value) -> bool {
        message.str("role") == Some(self.as_str())
    }

    pub(super) fn message(self, content: Value) -> Value {
        obj!({"role": self.as_str(), "content": content})
    }
}

/// The keys of a stale item a rebuilt part keeps: a custom call's kind and
/// Mistral's thinking part (`type`), or the field reasoning arrived in.
/// Signed data such as `reasoning_details` never survives an edit.
fn identity(item: &Value) -> Map<String, Value> {
    if let Some(kind @ ("custom" | "thinking")) = item.str("type") {
        return crate::json::object([("type".to_owned(), Value::from(kind))]);
    }
    ReasoningField::ALL
        .iter()
        .map(|field| field.as_str())
        .find(|key| item.get(*key).is_some_and(Value::is_string))
        .map_or_default(|key| crate::json::object([(key.to_owned(), Value::String(String::new()))]))
}

/// What the encoder knows about the target.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Context {
    quirks: Quirks,
    /// Whether the model reads user images.
    images: bool,
    reasoning_field: Option<ReasoningField>,
    /// The custom tools the request declares, whose calls are custom calls.
    custom: Vec<String>,
}

/// What media a host takes: content parts at all, media beyond images, and
/// video.
impl Context {
    fn parts(&self) -> bool {
        !matches!(self.quirks.vendor, V::DeepSeek | V::Mira)
    }

    fn files(&self) -> bool {
        self.parts() && !matches!(self.quirks.vendor, V::Perplexity | V::Cohere | V::Ollama)
    }

    fn video(&self) -> bool {
        self.files() && !matches!(self.quirks.vendor, V::OpenAi | V::Azure | V::Mistral)
    }
}

fn text_part(text: &str) -> Value {
    PartKind::Text.part(text.into())
}

/// The text of `parts`, one line each.
pub(super) fn joined_text(parts: &[Value]) -> String {
    let texts: Vec<&str> = parts.iter().filter_map(|part| part.str("text")).collect();
    texts.join("\n")
}

fn data_url(media: &Media, data: &str) -> String {
    format!("data:{};base64,{data}", media.media_type)
}

/// The `image_url` part an image goes as, when the host takes it in its
/// form; a user image names a detail level, `auto`.
fn image_url(media: &Media, ctx: &Context, user: bool) -> Option<Value> {
    let url = match &media.source {
        Source::Data { data } if ctx.parts() => data_url(media, data),
        // Ollama reads an image as data and never fetches a URL.
        Source::Url { url } if ctx.parts() && ctx.quirks.vendor != V::Ollama => url.clone(),
        Source::Data { .. } | Source::Url { .. } | Source::File { .. } => return None,
    };
    let mut part = obj!({"url": url});
    if user {
        part["detail"] = "auto".into();
    }
    Some(PartKind::ImageUrl.part(part))
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
fn media_part(media: &Media, ctx: &Context) -> Result<Value, &'static str> {
    let pdf = |file_data: String| obj!({"file_data": file_data, "filename": "document.pdf"});
    let kind = media.media_type.split('/').next().unwrap_or_default();
    match (kind, &media.source) {
        ("image", _) if !ctx.images => Err(USER_IMAGE_OMITTED),
        ("image", _) => image_url(media, ctx, true).ok_or(IMAGE_UNSENDABLE),
        ("audio", Source::Data { data }) if ctx.files() => Ok(PartKind::InputAudio
            .part(obj!({"data": data.as_str(), "format": audio_format(&media.media_type)}))),
        ("audio", _) => Err(AUDIO_UNSENDABLE),
        ("video", Source::Data { data }) if ctx.video() => {
            Ok(PartKind::VideoUrl.part(obj!({"url": data_url(media, data)})))
        }
        ("video", Source::Url { url }) if ctx.video() => {
            Ok(PartKind::VideoUrl.part(obj!({"url": url.as_str()})))
        }
        ("video", _) => Err(VIDEO_UNSENDABLE),
        (_, Source::File { id }) if ctx.files() && ctx.quirks.file_ids => {
            Ok(PartKind::File.part(obj!({"file_id": id.as_str()})))
        }
        _ if media.media_type != "application/pdf" => Err(DOCUMENT_UNSENDABLE),
        (_, Source::Data { data }) if ctx.files() => {
            Ok(PartKind::File.part(pdf(data_url(media, data))))
        }
        // OpenRouter and Mistral fetch a PDF a URL names.
        (_, Source::Url { url }) if matches!(ctx.quirks.vendor, V::OpenRouter | V::Mistral) => {
            Ok(PartKind::File.part(pdf(url.clone())))
        }
        _ => Err(DOCUMENT_UNSENDABLE),
    }
}

/// `note` after `parts`, unless the last part already says it.
fn note(parts: &mut Vec<Value>, note: &str) {
    if parts.last().and_then(|part| part.str("text")) != Some(note) {
        parts.push(text_part(note));
    }
}

/// A user message's parts: blank text says nothing, and several hosts
/// reject it.
fn user_parts(content: &[UserPart], ctx: &Context) -> Vec<Value> {
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
    if messages
        .last()
        .is_some_and(|message| Role::User.sent(message))
        && let Some(mut previous) = messages.pop()
    {
        let mut joined = match previous["content"].take() {
            Value::String(text) => vec![text_part(&text)],
            Value::Array(earlier) => earlier,
            _ => Vec::new(),
        };
        joined.append(&mut parts);
        parts = joined;
    }
    messages.push(Role::User.message(match parts.as_slice() {
        [part] if PartKind::of(part) == Some(PartKind::Text) => part["text"].clone(),
        _ => Value::Array(parts),
    }));
}

/// A result's parts: text, and the images the `tool` message can carry.
/// One the model reads only from a user goes to `attached`, leaving a note.
fn result_parts(result: &ToolResult, ctx: &Context, attached: &mut Vec<Value>) -> Vec<Value> {
    let content = match &result.output {
        Output::Text(text) => return vec![text_part(text)],
        Output::Json(value) => return vec![text_part(&value.to_string())],
        Output::Content(content) => content,
    };
    let mut parts = Vec::new();
    for part in content {
        match part {
            UserPart::Text(text) => parts.push(text_part(&text.text)),
            UserPart::Media(media) if media.media_type.starts_with("image/") => {
                let image = |user| image_url(media, ctx, user).filter(|_| ctx.images);
                if let Some(image) = image(false).filter(|_| ctx.quirks.image_tool_results) {
                    parts.push(image);
                } else if let Some(image) = image(true) {
                    attached.push(image);
                    note(&mut parts, TOOL_IMAGE_ATTACHED);
                } else {
                    note(&mut parts, TOOL_IMAGE_OMITTED);
                }
            }
            UserPart::Media(media) => {
                let placeholder = media_part(media, ctx).err().unwrap_or(DOCUMENT_UNSENDABLE);
                note(&mut parts, placeholder);
            }
        }
    }
    parts
}

/// The `tool` message that answers a call: its text joined into one
/// string, or its parts when the host asks for them or one is an image. A
/// result that says nothing says so.
fn tool_message(result: &ToolResult, ctx: &Context, attached: &mut Vec<Value>) -> Value {
    let mut parts = result_parts(result, ctx, attached);
    let image = |part: &Value| PartKind::of(part) == Some(PartKind::ImageUrl);
    let blank = parts
        .iter()
        .all(|part| !image(part) && part.str("text").is_none_or(|text| text.trim().is_empty()));
    if blank {
        let error = if result.error { "[tool error] " } else { "" };
        parts = vec![text_part(&format!("{error}{NO_TOOL_OUTPUT}"))];
    }
    let content = if ctx.quirks.tool_result_parts || parts.iter().any(image) {
        Value::Array(parts)
    } else {
        Value::String(joined_text(&parts))
    };
    let (role, id) = (Role::Tool.as_str(), result.id.as_str());
    obj!({"role": role, "tool_call_id": id, "content": content})
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

/// The messages of a conversation, a message at a time.
#[derive(Clone)]
pub(super) struct Wire {
    ctx: Context,
    /// The messages not settled: a trailing user message, which the next
    /// user message joins.
    open: Vec<Value>,
    /// Under a host that takes system text only where the conversation
    /// begins, the system texts: they go at its head, joined once a system
    /// message comes later.
    prompt: Vec<String>,
    /// Whether a message other than a system message came, and a system
    /// message after one.
    begun: bool,
    later: bool,
    /// Whether a tool message went.
    answered: bool,
    settled: usize,
}

impl Wire {
    fn new(ctx: Context) -> Self {
        Self {
            ctx,
            open: Vec::new(),
            prompt: Vec::new(),
            begun: false,
            later: false,
            answered: false,
            settled: 0,
        }
    }

    /// The messages sent only with this request: the head and the open
    /// tail, as the host takes them.
    fn finish(self) -> Result<(Vec<Value>, Vec<Value>), Error> {
        let system = |text: &str| Role::System.message(vec![text_part(text)].into());
        let mut head: Vec<Value> = if self.later {
            let joined = (!self.prompt.is_empty()).then(|| system(&self.prompt.join("\n\n")));
            joined.into_iter().collect()
        } else {
            self.prompt.iter().map(|text| system(text)).collect()
        };
        let mut tail = self.open;
        if head.is_empty() && tail.is_empty() && self.settled == 0 {
            return Err(Error::unsupported(
                "a Chat Completions request needs at least one message the host takes",
            ));
        }
        for message in head.iter_mut().chain(&mut tail) {
            rewrite::message(self.ctx.quirks.vendor, message);
        }
        Ok((head, tail))
    }
}

impl Fold for Wire {
    type Cx = Context;

    fn cx(&self) -> &Context {
        &self.ctx
    }

    fn settle(&mut self, out: &mut Vec<Value>) {
        let user = self
            .open
            .last()
            .is_some_and(|message| Role::User.sent(message));
        let settled = self.open.len() - usize::from(user);
        for mut message in self.open.drain(..settled) {
            rewrite::message(self.ctx.quirks.vendor, &mut message);
            out.push(message);
        }
        self.settled += settled;
    }

    fn push(&mut self, _: usize, message: &Message) -> Result<(), Error> {
        let ctx = &self.ctx;
        let messages = &mut self.open;
        if let Message::System(system) = message {
            let text = Some(system.text()).filter(|text| !text.trim().is_empty());
            if ctx.quirks.later_system == LaterSystem::Leading {
                self.prompt.extend(text);
                self.later |= self.begun;
            } else {
                let system = |text: String| Role::System.message(vec![text_part(&text)].into());
                messages.extend(text.map(system));
            }
            return Ok(());
        }
        self.begun = true;
        match message {
            Message::System(_) => {}
            Message::User(user) => push_user(messages, user_parts(&user.content, ctx)),
            Message::Tool(tool) if !ctx.quirks.supports_tools => {
                let parts = tool
                    .results
                    .iter()
                    .map(|result| text_part(&result_text(result)));
                push_user(messages, parts.collect());
            }
            Message::Tool(tool) => {
                let mut attached = Vec::new();
                for result in &tool.results {
                    messages.push(tool_message(result, ctx, &mut attached));
                    self.answered = true;
                }
                if !attached.is_empty() {
                    attached.insert(0, text_part(TOOL_IMAGES_HEADING));
                    push_user(messages, attached);
                }
            }
            Message::Assistant(assistant) => messages.extend(assistant_message(assistant, ctx)),
        }
        Ok(())
    }
}

/// One assistant message rebuilt from its parts: text joined into a
/// `content` string, or a part array when a part is a content part
/// (Mistral's thinking); reasoning under the field it arrived in, or the
/// host's, beside its item's other fields (`reasoning_details`); and each
/// call with its name and arguments. A message with neither content nor
/// calls is not sent.
fn assistant_message(assistant: &Assistant, ctx: &Context) -> Option<Value> {
    let (mut parts, mut has_parts) = (Vec::new(), false);
    // The reasoning texts by the field they go under; a message has few.
    let mut reasoning: Vec<(String, Vec<&str>)> = Vec::new();
    let (mut fields, mut calls) = (Map::new(), Vec::new());
    for part in &assistant.content {
        let sent = replay(part, KEY, identity);
        match part {
            AssistantPart::Text(block) => {
                parts.push(text_part(&block.text));
                // A text item is message fields: an answer's audio.
                if let Replay::Item(Value::Object(item)) = sent {
                    fields.extend(item);
                }
            }
            AssistantPart::Reasoning(block) => {
                let (field, item) = match sent {
                    Replay::Item(item) if item.get("type").is_some() => {
                        has_parts = true;
                        parts.push(item);
                        continue;
                    }
                    Replay::Identity(identity)
                        if identity.get("type") == Some(&json!("thinking")) =>
                    {
                        has_parts = true;
                        parts.push(PartKind::Thinking.part(vec![text_part(&block.text)].into()));
                        continue;
                    }
                    Replay::Item(item) => {
                        let mut strings = item.as_object().into_iter().flatten();
                        let field = strings.find(|(_, value)| value.is_string());
                        (field.map(|(key, _)| key.clone()), item)
                    }
                    Replay::Identity(identity) => (identity.keys().next().cloned(), Value::Null),
                    Replay::Rebuild => {
                        let field = ctx.reasoning_field.map(|f| f.as_str().to_owned());
                        (field, Value::Null)
                    }
                };
                let others = item.as_object().into_iter().flatten();
                merge_fields(&mut fields, others.filter(|(_, value)| !value.is_string()));
                if let Some(field) = field {
                    match reasoning.iter_mut().find(|(held, _)| *held == field) {
                        Some((_, texts)) => texts.push(&block.text),
                        None => reasoning.push((field, vec![&block.text])),
                    }
                }
            }
            // A host without tools reads a call as text.
            AssistantPart::ToolCall(call) if !ctx.quirks.supports_tools => {
                let input = Value::Object(call.input.clone());
                parts.push(text_part(&format!(
                    "[called tool {} with {input}]",
                    call.name
                )));
            }
            AssistantPart::ToolCall(call) => calls.push(call_item(call, sent, &ctx.custom)),
            // An opaque item is a content part; one with no `type` is not.
            AssistantPart::Opaque(opaque) if replays(part, KEY) => {
                if let Some(item) = (crate::replay::item(&opaque.meta, KEY))
                    .filter(|item| item.get("type").is_some())
                {
                    has_parts = true;
                    parts.push(item);
                }
            }
            AssistantPart::Opaque(_) | AssistantPart::Media(_) | AssistantPart::ToolResult(_) => {}
        }
    }
    let mut message = crate::json::object([("role".to_owned(), Role::Assistant.as_str().into())]);
    let text: String = parts.iter().filter_map(|part| part.str("text")).collect();
    if has_parts {
        message.insert("content".to_owned(), Value::Array(parts));
    } else if !text.is_empty() {
        message.insert("content".to_owned(), Value::String(text));
    }
    message.extend(
        reasoning
            .into_iter()
            .map(|(field, texts)| (field, texts.join("\n").into())),
    );
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
            .entry(field.as_str())
            .or_insert_with(|| Value::String(String::new()));
    }
    has_content.then_some(Value::Object(message))
}

/// `call` as the item the host takes: its current item, an edited one's
/// kind, or nothing. A call is custom when its item says so, or when it is
/// rebuilt and the request declares a custom tool by its name; its input is
/// then its arguments' `input`.
fn call_item(call: &ToolCall, sent: Replay, custom: &[String]) -> Value {
    let mut item = match sent {
        Replay::Item(Value::Object(item)) => item,
        Replay::Identity(identity) => identity,
        Replay::Item(_) | Replay::Rebuild => Map::new(),
    };
    let custom = if let Some(kind) = item.get("type").and_then(Value::as_str) {
        kind == "custom"
    } else {
        let declared = custom.iter().any(|tool| *tool == call.name);
        let kind = if declared { "custom" } else { "function" };
        item.insert("type".to_owned(), Value::from(kind));
        declared
    };
    item.insert("id".to_owned(), call.id.as_str().into());
    let (slot, key, value) = if custom {
        let input = match call.input.get("input") {
            Some(Value::String(input)) => input.clone(),
            Some(input) => input.to_string(),
            None => String::new(),
        };
        ("custom", "input", input)
    } else {
        (
            "function",
            "arguments",
            Value::Object(call.input.clone()).to_string(),
        )
    };
    let slot = item.entry(slot).or_insert_with(|| json!({}));
    if !slot.is_object() {
        *slot = json!({});
    }
    slot["name"] = call.name.as_str().into();
    slot[key] = value.into();
    Value::Object(item)
}

fn function_tool(tool: &Tool, strict_by_default: bool) -> Value {
    let strict = tool.strict.unwrap_or(strict_by_default);
    let mut parameters = tool.schema.clone();
    if strict {
        schema::openai(&mut parameters);
    }
    let (name, description) = (tool.name.as_str(), tool.description.as_str());
    let mut function = obj!({"name": name, "description": description, "parameters": parameters});
    if strict {
        function["strict"] = Value::Bool(true);
    }
    obj!({"type": "function", "function": function})
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
    let Format::Json { schema, name, .. } = format else {
        return None;
    };
    let Some(schema) = schema else {
        return Some(json!({"type": "json_object"}));
    };
    let mut schema = schema.clone();
    schema::openai(&mut schema);
    let name = name
        .as_deref()
        .or_else(|| schema.str("title"))
        .unwrap_or("response_schema")
        .to_owned();
    let json_schema = obj!({"name": name, "strict": true, "schema": schema});
    Some(obj!({"type": "json_schema", "json_schema": json_schema}))
}

/// The request body for `request` to `target`.
pub(super) fn body(
    quirks: &Quirks,
    target: &Target<'_>,
    request: &Request,
) -> Result<(Value, Option<History>), Error> {
    let model = target.model;
    let mut options = request.options_for(KEY);
    // The request's own tools, function or native (Groq's
    // `browser_search`), join the typed ones.
    let passthrough = match options.remove("tools") {
        Some(Value::Array(tools)) => tools,
        _ => Vec::new(),
    };
    let ctx = Context {
        quirks: *quirks,
        images: rules::reads_images(quirks.vendor, model),
        reasoning_field: rules::reasoning_field(quirks, &target.provider.base_url, model),
        custom: passthrough
            .iter()
            .filter(|tool| tool.str("type") == Some("custom"))
            .filter_map(|tool| tool.at("/custom/name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect(),
    };
    let (wire, settled) = conversation::fold(&request.messages, target, Wire::new(ctx))?;
    // A format waits for the first tool result unless the host takes both
    // at once, so the model can still call a tool.
    let answered = wire.answered;
    let (head, tail) = wire.finish()?;
    let typed = request.tools.iter().filter(|_| quirks.supports_tools);
    let mut tools: Vec<Value> = typed
        .map(|tool| function_tool(tool, quirks.strict_tools))
        .collect();
    tools.extend(passthrough);
    let offers_tools = quirks.supports_tools && !tools.is_empty();
    if offers_tools
        && matches!(quirks.vendor, V::OpenAi | V::Azure)
        && !openai::chat_calls_tools(model)
    {
        return Err(Error::unsupported(
            "the model cannot call tools through Chat Completions; use the Responses API",
        ));
    }
    let format = request
        .format
        .as_ref()
        .filter(|_| quirks.supports_response_format)
        .filter(|_| quirks.response_format_with_tools || tools.is_empty() || answered)
        .and_then(response_format);
    let choice = (offers_tools && request.tool_choice != ToolChoice::Auto)
        .then(|| tool_choice(&request.tool_choice));
    let generation = rules::generation(
        quirks,
        model,
        &request.generation,
        request.parallel_tool_calls,
        !tools.is_empty(),
    );
    let cache_key = request
        .cache_key
        .as_ref()
        .filter(|_| quirks.vendor == V::OpenAi);
    let mut body = obj!({
        "model": (quirks.routing == Routing::Path).then_some(model),
        "response_format": format,
        "tool_choice": choice,
        "tools": (!tools.is_empty()).then_some(tools),
        "prompt_cache_key": cache_key.map(String::as_str),
        "stream": true,
    });
    if let Some(fields) = body.as_object_mut() {
        // Each field left out is `null` here.
        fields.retain(|_, value| !value.is_null());
        fields.extend(generation);
    }
    let mut history = (!options.contains_key("messages")).then_some(History {
        key: "messages",
        head,
        settled,
        tail,
    });
    crate::json::overlay(&mut body, &options);
    if let Some(fields) = body.as_object_mut() {
        rewrite::finish(quirks, fields, history.as_mut())?;
    }
    Ok((body, history))
}
