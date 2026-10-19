//! A request as a Responses body. Turns this protocol produced replay their
//! provider items verbatim, an edited part keeps its item's identity, and
//! everything else is rebuilt from the shared fields.

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{Instructions, ItemType, KEY, PartType, Responses, Status};
use crate::conversation::{self, Fold, History};
use crate::error::Error;
use crate::json::Lenient;
use crate::models::openai;
use crate::protocol::Target;
use crate::protocols::schema;
use crate::replay::{Replay, replay, replays};
use crate::spec::{
    AssistantPart, Effort, Format, Message, Output, Request, SmolStr, Source, Tool, ToolChoice,
    UserPart,
};

/// The request body but its `input` and `tools`, which move in after
/// rather than serialize as copies; the caller's options are laid over it.
#[derive(Serialize)]
struct Body<'a> {
    model: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<[&'static str; 1]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
}

/// The request body for `request` to `target`.
pub(crate) fn body(
    responses: &Responses,
    target: &Target<'_>,
    request: &Request,
) -> Result<(Value, Option<History>), Error> {
    let (model, generation) = (target.model, &request.generation);
    let mut options = request.options_for(KEY);
    // Stateless unless the caller keeps the conversation on the provider:
    // every turn is sent whole, and reasoning replays from its ciphertext.
    let stateless = options.get("store").and_then(Value::as_bool) != Some(true)
        && !options.contains_key("previous_response_id")
        && !options.contains_key("conversation");
    let cx = Cx {
        stateless,
        instructions: responses.quirks.instructions,
    };
    let (input, settled) = conversation::fold(&request.messages, target, Input::new(cx))?;
    let (instructions, tail) = input.finish()?;
    let strict = responses.quirks.strict_tools;
    let mut tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| function_tool(tool, strict))
        .collect();
    // Hosted tools go with the rest; the caller's other options go last.
    if let Some(Value::Array(hosted)) = options.remove("tools") {
        tools.extend(hosted);
    }
    let tooled = !tools.is_empty();
    // Reasoning models reject sampling while they reason: dropped rather
    // than failing a request that would otherwise succeed.
    let sampling = openai::sampling(model).takes(generation.reasoning);
    let tool_choice = match &request.tool_choice {
        _ if !tooled => None,
        ToolChoice::Auto => None,
        ToolChoice::None => Some(json!("none")),
        ToolChoice::Required => Some(json!("required")),
        ToolChoice::Tool(name) => Some(json!({"type": "function", "name": name})),
    };
    let body = Body {
        model,
        stream: true,
        instructions,
        max_output_tokens: generation.max_output_tokens.map(std::num::NonZeroU32::get),
        temperature: generation.temperature.filter(|_| sampling),
        top_p: generation.top_p.filter(|_| sampling),
        tool_choice,
        parallel_tool_calls: request.parallel_tool_calls.filter(|_| tooled),
        text: request.format.as_ref().and_then(text_format),
        reasoning: reasoning(model, generation.reasoning),
        store: stateless.then_some(false),
        include: (stateless && openai::reasons(model) != Some(false))
            .then_some(["reasoning.encrypted_content"]),
        prompt_cache_key: request.cache_key.as_deref(),
    };
    let mut body =
        serde_json::to_value(body).map_err(|error| Error::unsupported(error.to_string()))?;
    let history = (!options.contains_key("input")).then(|| History {
        key: "input",
        head: Vec::new(),
        settled,
        tail,
    });
    if let Some(fields) = body.as_object_mut() {
        if tooled {
            fields.insert("tools".to_owned(), tools.into());
        }
        fields.extend(options);
    }
    Ok((body, history))
}

/// The keys of a stale item a rebuilt part keeps.
fn identity(item: &Value) -> Map<String, Value> {
    let keys: &[&str] = match ItemType::of(item.get("type")) {
        Some(ItemType::Message) => &["type", "id", "phase"],
        Some(ItemType::Reasoning) => &["type", "id", "encrypted_content"],
        Some(ItemType::FunctionCall | ItemType::CustomToolCall) => &["type", "id"],
        _ => &[],
    };
    crate::json::object(
        keys.iter()
            .filter_map(|key| Some(((*key).to_owned(), item.get(*key)?.clone()))),
    )
}

fn unsendable(part: &str) -> Error {
    Error::unsupported(format!("the Responses API cannot carry this {part}"))
}

/// The content part a user part goes as: text, an image, or an
/// `input_file`.
fn user_part(part: &UserPart) -> Result<Value, Error> {
    let media = match part {
        UserPart::Text(text) => return Ok(json!({"type": PartType::InputText, "text": text.text})),
        UserPart::Media(media) => media,
    };
    let kind = &media.media_type;
    if kind.starts_with("image/") {
        let (key, source) = match &media.source {
            Source::Data { data } => ("image_url", format!("data:{kind};base64,{data}")),
            Source::Url { url } => ("image_url", url.clone()),
            Source::File { id } => ("file_id", id.to_string()),
        };
        return Ok(json!({"type": PartType::InputImage, "detail": "auto", key: source}));
    }
    let (key, source) = match &media.source {
        Source::File { id } => ("file_id", id.to_string()),
        // `input_file` reads the type of a file it fetches itself.
        Source::Url { url } => ("file_url", url.clone()),
        Source::Data { data } if kind == "application/pdf" => {
            let data = format!("data:application/pdf;base64,{data}");
            let filename = "document.pdf";
            return Ok(
                json!({"type": PartType::InputFile, "file_data": data, "filename": filename}),
            );
        }
        Source::Data { .. } => return Err(unsendable(&format!("{kind} file"))),
    };
    Ok(json!({"type": PartType::InputFile, key: source}))
}

/// A tool result's `output`: one text as a string, anything else as its
/// ordered parts.
fn result_output(output: &Output) -> Result<Value, Error> {
    Ok(match output {
        Output::Text(text) => text.as_str().into(),
        Output::Json(value) => value.to_string().into(),
        Output::Content(parts) => match parts.as_slice() {
            [UserPart::Text(text)] => text.text.as_str().into(),
            parts => parts.iter().map(user_part).collect::<Result<_, _>>()?,
        },
    })
}

/// What a Responses conversation's items read besides its messages.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cx {
    /// A stateless request resolves no stored item, so reasoning without
    /// its ciphertext is left out, and the part after it is rebuilt with no
    /// item id, as another model's turn is.
    stateless: bool,
    instructions: Instructions,
}

/// The input items of a conversation, a message at a time. System messages
/// that go to `instructions` are lifted out as they come; a leading one
/// waits for the first item that is not one to know whether it goes.
#[derive(Clone)]
pub(crate) struct Input {
    cx: Cx,
    /// The call ids the request sends as custom calls, which are rare.
    custom: Vec<SmolStr>,
    /// Items not settled: the leading system messages, while nothing else
    /// came.
    open: Vec<Value>,
    /// The system texts lifted to `instructions`.
    lifted: Vec<String>,
    /// How many items settled.
    settled: usize,
    /// Whether an item that is not a system message came.
    begun: bool,
}

impl Input {
    fn new(cx: Cx) -> Self {
        Self {
            cx,
            custom: Vec::new(),
            open: Vec::new(),
            lifted: Vec::new(),
            settled: 0,
            begun: false,
        }
    }

    /// An item that is not a system message; the leading ones before it go
    /// to `instructions` where they lead.
    fn item(&mut self, item: Value) {
        if !std::mem::replace(&mut self.begun, true)
            && self.cx.instructions == Instructions::Leading
        {
            let leading = self
                .open
                .drain(..)
                .map(|item| system_text(&item).to_owned());
            self.lifted.extend(leading);
        }
        self.open.push(item);
    }

    /// The instructions lifted, and the items left open: a request of only
    /// system messages keeps them as input, which must not be empty.
    fn finish(self) -> Result<(String, Vec<Value>), Error> {
        if self.settled + self.open.len() == 0 {
            let what = if self.lifted.is_empty() {
                "input item"
            } else {
                "non-system input item"
            };
            let message = format!("a Responses request needs at least one {what}");
            return Err(Error::unsupported(message));
        }
        let mut lifted = self.lifted;
        lifted.retain(|text| !text.is_empty());
        Ok((lifted.join("\n\n"), self.open))
    }
}

/// The text of a system message's item, trimmed.
fn system_text(item: &Value) -> &str {
    let text = item.at("/content/0/text").and_then(Value::as_str);
    text.unwrap_or_default().trim()
}

impl Fold for Input {
    type Cx = Cx;

    fn cx(&self) -> &Cx {
        &self.cx
    }

    fn settle(&mut self, out: &mut Vec<Value>) {
        if self.begun || self.cx.instructions != Instructions::Leading {
            self.settled += self.open.len();
            out.append(&mut self.open);
        }
    }

    fn push(&mut self, position: usize, turn: &Message) -> Result<(), Error> {
        let message = |role: &str, part: Value| {
            let mut message = json!({"type": ItemType::Message, "role": role});
            message["content"] = vec![part].into();
            message
        };
        let parts = match turn {
            Message::System(system) => {
                let text = system.text();
                if self.cx.instructions == Instructions::All {
                    self.lifted.push(text.trim().to_owned());
                } else {
                    let text = json!({"type": PartType::InputText, "text": text});
                    self.open.push(message("system", text));
                }
                return Ok(());
            }
            Message::User(user) => {
                for part in &user.content {
                    // Blank text says nothing, and some backends reject it.
                    if let UserPart::Text(text) = part
                        && text.text.trim().is_empty()
                    {
                        continue;
                    }
                    self.item(message("user", user_part(part)?));
                }
                return Ok(());
            }
            // A function output has no error field: a failed result says so
            // in its text.
            Message::Tool(tool) => {
                for result in &tool.results {
                    let (id, output) = (&result.id, result_output(&result.output)?);
                    let mut item = if self.custom.contains(id) {
                        json!({"type": ItemType::CustomToolCallOutput, "call_id": id})
                    } else {
                        json!({"type": ItemType::FunctionCallOutput, "call_id": id, "status": Status::Completed})
                    };
                    item["output"] = output;
                    self.item(item);
                }
                return Ok(());
            }
            Message::Assistant(assistant) => &assistant.content,
        };
        let stateless = self.cx.stateless;
        let (mut texts, mut unpaired) = (0, false);
        for part in parts {
            let reasoning = matches!(part, AssistantPart::Reasoning(_));
            let sent = if unpaired && !reasoning {
                unpaired = false;
                Replay::Rebuild
            } else {
                replay(part, KEY, identity)
            };
            let stated = |key: &str| match &sent {
                Replay::Item(item) => item.get(key),
                Replay::Identity(kept) => kept.get(key),
                Replay::Rebuild => None,
            };
            let ciphered = stated("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|cipher| !cipher.is_empty());
            let kind = ItemType::of(stated("type"));
            if stateless && reasoning && sent != Replay::Rebuild && !ciphered {
                unpaired = true;
                continue;
            }
            if let AssistantPart::ToolCall(call) = part {
                self.custom.retain(|custom| *custom != call.id);
                if kind == Some(ItemType::CustomToolCall) {
                    self.custom.push(call.id.clone());
                }
            }
            let kept = match sent {
                Replay::Item(item) => {
                    let mut item = item.clone();
                    // A streamed item states unset fields as `null` (a
                    // reasoning item's `status`), which the input side
                    // rejects as unknown.
                    if let Some(fields) = item.as_object_mut() {
                        fields.retain(|_, value| !value.is_null());
                    }
                    if let AssistantPart::ToolCall(call) = part {
                        // The call takes the id its result answers.
                        item["call_id"] = call.id.as_str().into();
                    }
                    self.item(item);
                    continue;
                }
                Replay::Identity(kept) => kept,
                Replay::Rebuild => Map::new(),
            };
            if let Some(item) = rebuilt(part, &kept, kind, position, texts)? {
                self.item(item);
            }
            texts += usize::from(matches!(part, AssistantPart::Text(_)));
        }
        Ok(())
    }
}

/// A part rebuilt from its shared fields, keeping `identity` of its stale
/// item, whose type is `kind`; it is text part `text` of the message at
/// `position`. `None` for a part that cannot go without its item.
fn rebuilt(
    part: &AssistantPart,
    identity: &Map<String, Value>,
    kind: Option<ItemType>,
    position: usize,
    text: usize,
) -> Result<Option<Value>, Error> {
    let id = |prefix: &str| {
        let id = identity.get("id").and_then(Value::as_str);
        id.filter(|id| !id.is_empty() && id.starts_with(prefix) && id.len() <= 64)
    };
    Ok(Some(match part {
        AssistantPart::Text(text_part) => {
            let synthetic = match text {
                0 => format!("msg_a_{position}"),
                n => format!("msg_a_{position}_{n}"),
            };
            let content =
                [json!({"type": PartType::OutputText, "text": text_part.text, "annotations": []})];
            let mut item = json!({
                "type": ItemType::Message,
                "role": "assistant",
                "content": content,
                "status": Status::Completed,
                "id": id("").map_or(synthetic, str::to_owned),
            });
            if let Some(phase) = identity.get("phase").filter(|phase| phase.is_string()) {
                item["phase"] = phase.clone();
            }
            item
        }
        AssistantPart::ToolCall(call) => {
            let arguments = Value::Object(call.input.clone()).to_string();
            let (kind, key, value, prefix) = if kind == Some(ItemType::CustomToolCall) {
                let input = call.input.get("input").and_then(Value::as_str);
                let input = input.map_or(arguments, str::to_owned);
                (ItemType::CustomToolCall, "input", input, "ctc_")
            } else {
                (ItemType::FunctionCall, "arguments", arguments, "fc_")
            };
            let mut item = json!({"type": kind, "call_id": call.id, "name": call.name, key: value});
            if let Some(id) = id(prefix) {
                item["id"] = id.into();
            }
            item
        }
        // An edited reasoning part keeps its item's id and ciphertext, so
        // the items after it stay paired.
        AssistantPart::Reasoning(reasoning) => {
            let Some(id) = id("") else {
                return Ok(None);
            };
            let summary = (!reasoning.text.is_empty())
                .then(|| json!({"type": PartType::SummaryText, "text": reasoning.text}));
            let mut item =
                json!({"type": ItemType::Reasoning, "id": id, "summary": Vec::from_iter(summary)});
            if let Some(ciphertext) = identity.get("encrypted_content") {
                item["encrypted_content"] = ciphertext.clone();
            }
            item
        }
        AssistantPart::Opaque(opaque) if replays(part, KEY) => {
            let state = opaque.meta.get(KEY);
            return Ok(state.and_then(|state| state.get("item")).cloned());
        }
        AssistantPart::Opaque(_) | AssistantPart::ToolResult(_) => return Ok(None),
        AssistantPart::Media(_) => return Err(unsendable("assistant image")),
    }))
}

fn function_tool(tool: &Tool, strict_by_default: bool) -> Value {
    let strict = tool.strict.unwrap_or(strict_by_default);
    let mut parameters = tool.schema.clone();
    if strict {
        schema::openai(&mut parameters);
    }
    // An omitted `strict` means "attempt strict mode", so `false` must reach
    // the wire.
    let mut definition =
        json!({"type": "function", "name": tool.name, "parameters": parameters, "strict": strict});
    if !tool.description.is_empty() {
        definition["description"] = tool.description.as_str().into();
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
    let Some(mut json_schema) = json_schema.clone() else {
        return Some(json!({"format": {"type": "json_object"}}));
    };
    schema::openai(&mut json_schema);
    let name = name.as_deref().unwrap_or("response_schema");
    let mut format =
        json!({"type": "json_schema", "name": name, "schema": json_schema, "strict": true});
    if let Some(description) = description {
        format["description"] = json!(description);
    }
    Some(json!({ "format": format }))
}

/// The `reasoning` field: the effort asked for, with summaries so reasoning
/// streams. A model that does not reason takes none, and one that cannot
/// turn reasoning off takes no effort `none`.
fn reasoning(model: &str, effort: Option<Effort>) -> Option<Value> {
    match (effort, openai::reasons(model)) {
        (Some(Effort::None), _) if !openai::responses_can_disable(model) => None,
        (_, Some(false)) | (None, None) => None,
        (Some(Effort::None), _) => Some(json!({"effort": "none"})),
        (Some(effort), _) => Some(json!({"effort": effort.as_str(), "summary": "auto"})),
        (None, Some(true)) => Some(json!({"summary": "auto"})),
    }
}
