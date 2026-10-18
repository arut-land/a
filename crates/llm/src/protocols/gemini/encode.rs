//! A request as a `GenerateContent` body. System text goes in the
//! `systemInstruction`, the rest in `contents`; parts this protocol produced
//! go back as Gemini sent them, and everything else is rebuilt.
//!
//! Options a model cannot take are left out rather than failing the
//! request: an effort a model has no level for, a budget out of its range,
//! turning off thinking a model cannot turn off, more than 5 stop
//! sequences, a seed beyond 32 bits, and `parallel_tool_calls: false`.

use serde_json::{Map, Value, json};

use super::{Gemini, KEY, ThoughtReplay};
use crate::error::Error;
use crate::json::Lenient;
use crate::models::google as model;
use crate::models::{Reasoning, Thinking};
use crate::protocol::Target;
use crate::replay::{Replay, replay, replays};
use crate::spec::{
    AssistantPart, Effort, Format, Generation, Media, Message, Output, Request, Source, ToolChoice,
    ToolResult, UserPart,
};

/// What leads the user content a tool result's images move to, for a model
/// that reads no image inside a function response.
pub(crate) const RESULT_IMAGES: &str = "Attached image(s) from tool result:";

/// What a tool result that says nothing says instead: models answer it
/// better than an empty one.
pub(crate) const NO_OUTPUT: &str = "(no tool output)";

/// Rebuilt calls carry Google's placeholder signature on Gemini 3, which
/// rejects a call it did not sign without one.
const UNSIGNED_CALL: &str = "skip_thought_signature_validator";

fn text_part(text: &str) -> Value {
    json!({"text": text, "thought": false})
}

/// `media` as part data: a URL by reference, data inline.
fn media_data(media: &Media) -> Result<Value, Error> {
    Ok(match &media.source {
        Source::Data { data } => {
            json!({"inlineData": {"mimeType": media.media_type, "data": data}})
        }
        Source::Url { url } => json!({"fileData": {"mimeType": media.media_type, "fileUri": url}}),
        Source::File { .. } => {
            return Err(Error::unsupported(
                "Gemini cannot carry media by a file id; send its URI as a URL",
            ));
        }
    })
}

/// `media` as a whole part, marked as no thought.
fn media_part(media: &Media) -> Result<Value, Error> {
    let mut part = media_data(media)?;
    part["thought"] = Value::Bool(false);
    Ok(part)
}

/// What a function response may hold inline: JPEG, PNG or WEBP data.
fn inline_image(media: &Media) -> bool {
    matches!(media.source, Source::Data { .. })
        && matches!(
            media.media_type.as_str(),
            "image/jpeg" | "image/png" | "image/webp"
        )
}

/// Whether `part` is a function response.
fn is_response(part: &Value) -> bool {
    part.get("functionResponse").is_some()
}

/// The contents being built. Adjacent user messages are one content, but
/// function responses and other user parts go in contents of their own:
/// Gemini answers text that shares a content with responses poorly.
#[derive(Default)]
struct Contents(Vec<Value>);

impl Contents {
    fn push(&mut self, role: &str, parts: Vec<Value>) {
        // Gemini rejects a content with no parts.
        if parts.is_empty() {
            return;
        }
        if role == "user"
            && let Some(last) = self.0.last_mut()
            && last.str("role") == Some("user")
            && let Some(held) = last.get_mut("parts").and_then(Value::as_array_mut)
            && held.last().map(is_response) == parts.first().map(is_response)
        {
            held.extend(parts);
            return;
        }
        self.0.push(json!({"parts": parts, "role": role}));
    }
}

/// The contents and system instruction parts of `messages` for `model`.
pub(super) fn contents(
    messages: &[Message],
    model: &str,
) -> Result<(Vec<Value>, Vec<Value>), Error> {
    let ids = model::takes_call_ids(model);
    let mut system = Vec::new();
    let mut contents = Contents::default();
    for message in messages {
        match message {
            // Gemini takes system text only in its instruction, so a later
            // system message joins the leading one.
            Message::System(text) => {
                let text = text.text();
                if !text.trim().is_empty() {
                    system.push(text_part(&text));
                }
            }
            Message::User(user) => {
                let mut parts = Vec::new();
                for part in &user.content {
                    match part {
                        // Blank text says nothing, and Gemini rejects it.
                        UserPart::Text(text) if text.text.trim().is_empty() => {}
                        UserPart::Text(text) => parts.push(text_part(&text.text)),
                        UserPart::Media(media) => parts.push(media_part(media)?),
                    }
                }
                contents.push("user", parts);
            }
            Message::Tool(tool) => {
                let mut moved = Vec::new();
                let parts = tool
                    .results
                    .iter()
                    .map(|result| function_response(result, model, ids, &mut moved))
                    .collect::<Result<Vec<_>, _>>()?;
                contents.push("user", parts);
                if !moved.is_empty() {
                    let mut attached = vec![text_part(RESULT_IMAGES)];
                    attached.extend(moved);
                    contents.push("user", attached);
                }
            }
            Message::Assistant(assistant) => {
                let mut parts = Vec::new();
                for part in &assistant.content {
                    if let Some(part) = assistant_part(part, model, ids)? {
                        parts.push(part);
                    }
                }
                contents.push("model", parts);
            }
        }
    }
    Ok((contents.0, system))
}

/// A tool result as a function response. Its error says so under the
/// `error` key, as Gemini's SDKs spell a failed function. Media it may not
/// hold inline moves to `moved`, for a user content after the responses.
fn function_response(
    result: &ToolResult,
    model: &str,
    ids: bool,
    moved: &mut Vec<Value>,
) -> Result<Value, Error> {
    let images = model::reads_result_images(model);
    let (mut values, mut inline) = (Vec::new(), Vec::new());
    match &result.output {
        Output::Text(text) => values.push(Value::String(text.clone())),
        Output::Json(value) => values.push(value.clone()),
        Output::Content(parts) => {
            for part in parts {
                match part {
                    UserPart::Text(text) => values.push(Value::String(text.text.clone())),
                    UserPart::Media(media) if images && inline_image(media) => {
                        inline.push(media_data(media)?);
                    }
                    UserPart::Media(media) => moved.push(media_part(media)?),
                }
            }
        }
    }
    let blank = values
        .iter()
        .all(|value| value.as_str().is_some_and(|text| text.trim().is_empty()));
    if blank && inline.is_empty() {
        let text = if result.error {
            format!("[tool error] {NO_OUTPUT}")
        } else {
            NO_OUTPUT.to_owned()
        };
        values = vec![Value::String(text)];
    }
    // Gemini 2 rejects a result of several parts: its texts join.
    if !images && values.len() > 1 {
        let joined: Vec<String> = values
            .iter()
            .map(|value| match value {
                Value::String(text) => text.clone(),
                value => value.to_string(),
            })
            .collect();
        values = vec![Value::String(joined.join("\n"))];
    }
    let key = if result.error { "error" } else { "result" };
    let mut response = Map::new();
    response.insert("name".into(), json!(result.name));
    if ids {
        response.insert("id".into(), json!(result.id));
    }
    let value = match values.len() {
        0 => None,
        1 => values.pop(),
        _ => Some(Value::Array(values)),
    };
    if let Some(value) = value {
        response.insert("response".into(), json!({ key: value }));
    }
    if !inline.is_empty() {
        response.insert("parts".into(), Value::Array(inline));
    }
    Ok(json!({"functionResponse": response, "thought": false}))
}

/// Whether `text` is padded standard base64.
fn is_base64(text: &str) -> bool {
    let body = text.trim_end_matches('=');
    let alphabet = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/');
    !body.is_empty()
        && text.len().is_multiple_of(4)
        && text.len() - body.len() <= 2
        && body.bytes().all(alphabet)
}

/// One assistant part as a Gemini part: its item while that is current,
/// else rebuilt from its shared fields; `None` when it has nothing to send.
fn assistant_part(part: &AssistantPart, model: &str, ids: bool) -> Result<Option<Value>, Error> {
    if let AssistantPart::Opaque(opaque) = part {
        let item = opaque.meta.get(KEY).and_then(|state| state.get("item"));
        return Ok(item.filter(|_| replays(part, KEY)).cloned());
    }
    if let Replay::Item(item) = replay(part, KEY, |_| Map::new()) {
        let mut item = item.clone();
        if let Some(fields) = item.as_object_mut() {
            if let (AssistantPart::ToolCall(call), Some(Value::Object(function))) =
                (part, fields.get_mut("functionCall"))
            {
                // The call takes the id its response answers.
                if ids {
                    function.insert("id".into(), json!(call.id));
                } else {
                    function.remove("id");
                }
            }
            // Gemini rejects the whole request over a signature that is not
            // base64.
            if fields
                .get("thoughtSignature")
                .and_then(Value::as_str)
                .is_some_and(|signature| !is_base64(signature))
            {
                fields.remove("thoughtSignature");
            }
        }
        return Ok(Some(item));
    }
    Ok(Some(match part {
        AssistantPart::Text(text) if text.text.trim().is_empty() => return Ok(None),
        AssistantPart::Text(text) => json!({"text": text.text}),
        AssistantPart::Reasoning(reasoning)
            if reasoning.redacted || reasoning.text.trim().is_empty() =>
        {
            return Ok(None);
        }
        AssistantPart::Reasoning(reasoning) => json!({"thought": true, "text": reasoning.text}),
        AssistantPart::ToolCall(call) => {
            let mut function = json!({"name": call.name, "args": call.input});
            if ids {
                function["id"] = json!(call.id);
            }
            let mut part = json!({ "functionCall": function });
            if model::validates_signatures(model) {
                part["thoughtSignature"] = json!(UNSIGNED_CALL);
            }
            part
        }
        AssistantPart::Media(media) => media_part(media)?,
        AssistantPart::ToolResult(_) | AssistantPart::Opaque(_) => return Ok(None),
    }))
}

/// Drop the thought signatures of every part before the newest user text:
/// the turns the model has finished. Only the signatures go.
pub(crate) fn drop_finished_signatures(contents: &mut [Value]) {
    let current = contents.iter().rposition(|content| {
        let parts = content.arr("parts");
        content.str("role") == Some("user")
            && parts.iter().any(|part| part.get("text").is_some())
            && !parts.iter().any(is_response)
    });
    for content in contents.iter_mut().take(current.unwrap_or(0)) {
        let parts = content.get_mut("parts").and_then(Value::as_array_mut);
        for part in parts.into_iter().flatten().filter_map(Value::as_object_mut) {
            part.remove("thoughtSignature");
        }
    }
}

fn tool_config(choice: &ToolChoice) -> Value {
    let config = match choice {
        ToolChoice::Auto => json!({"mode": "AUTO"}),
        ToolChoice::None => json!({"mode": "NONE"}),
        ToolChoice::Required => json!({"mode": "ANY"}),
        ToolChoice::Tool(name) => json!({"mode": "ANY", "allowed_function_names": [name]}),
    };
    json!({ "functionCallingConfig": config })
}

/// A tool as its function declaration. Its JSON Schema goes whole as
/// `parametersJsonSchema`, which reads full JSON Schema.
fn declaration(tool: &crate::spec::Tool) -> Value {
    let mut declaration = json!({"name": tool.name});
    if !tool.description.is_empty() {
        declaration["description"] = json!(tool.description);
    }
    if !tool.schema.is_null() {
        declaration["parametersJsonSchema"] = tool.schema.clone();
    }
    declaration
}

/// The `thinkingConfig` `generation` asks of `model`, with thought
/// summaries whenever thinking stays on, so reasoning streams.
fn thinking_config(model: &str, generation: &Generation) -> Option<Value> {
    let reasoning = model::reasoning(model);
    let effort = generation.reasoning;
    let budget = generation.reasoning_budget.map(std::num::NonZeroU32::get);
    let level = |effort: Effort| json!({"thinkingLevel": effort.as_str()});
    let config = match (effort, reasoning) {
        (_, Reasoning::None) => return None,
        (
            Some(Effort::None),
            Reasoning::Takes(Thinking {
                can_disable: true,
                budget: by_budget,
                ..
            }),
        ) => {
            return Some(if by_budget.is_some() {
                json!({"thinkingBudget": 0})
            } else {
                json!({"thinkingLevel": "minimal"})
            });
        }
        // Thinking it cannot turn off goes on as the model's default.
        (Some(Effort::None), _) => None,
        // Gemini 2.5 takes a budget, and no table turns an effort into one.
        (
            _,
            Reasoning::Takes(Thinking {
                budget: Some(range),
                ..
            }),
        ) => budget
            .filter(|budget| range.contains(*budget))
            .map(|budget| json!({"thinkingBudget": budget})),
        (Some(effort), Reasoning::Takes(thinking)) if thinking.takes(effort) => Some(level(effort)),
        // Gemini's levels stop at `high`.
        (
            Some(effort @ (Effort::Minimal | Effort::Low | Effort::Medium | Effort::High)),
            Reasoning::Unknown,
        ) => Some(level(effort)),
        (_, Reasoning::Unknown) => budget.map(|budget| json!({"thinkingBudget": budget})),
        (_, Reasoning::Takes(_)) => None,
    };
    let summarizes = matches!(reasoning, Reasoning::Takes(thinking)
        if thinking.budget.is_some() || !thinking.efforts.is_empty());
    let mut config = config.unwrap_or_else(|| json!({}));
    if summarizes {
        config["includeThoughts"] = Value::Bool(true);
    }
    (config.as_object().is_some_and(|config| !config.is_empty())).then_some(config)
}

fn generation_config(model: &str, request: &Request) -> Map<String, Value> {
    let generation = &request.generation;
    let mut config = Map::new();
    if let Some(Format::Json { schema, .. }) = &request.format {
        config.insert("responseMimeType".into(), json!("application/json"));
        if let Some(schema) = schema {
            config.insert("responseJsonSchema".into(), schema.clone());
        }
    }
    if let Some(temperature) = generation.temperature {
        config.insert("temperature".into(), json!(temperature));
    }
    if let Some(max) = generation.max_output_tokens {
        config.insert("maxOutputTokens".into(), json!(max.get()));
    }
    if let Some(top_p) = generation.top_p {
        config.insert("topP".into(), json!(top_p));
    }
    if let Some(top_k) = generation.top_k {
        config.insert("topK".into(), json!(top_k.get()));
    }
    if let Some(seed) = generation.seed.and_then(|seed| i32::try_from(seed).ok()) {
        config.insert("seed".into(), json!(seed));
    }
    if (1..=5).contains(&generation.stop.len()) {
        config.insert("stopSequences".into(), json!(generation.stop));
    }
    if let Some(thinking) = thinking_config(model, generation) {
        config.insert("thinkingConfig".into(), thinking);
    }
    config
}

/// The request body for `request` to `target`. This protocol's options are
/// body fields merged over it last: their `tools` join the declared ones,
/// and objects such as `generationConfig` merge key by key.
pub(crate) fn body(
    gemini: &Gemini,
    target: &Target<'_>,
    request: &Request,
) -> Result<Value, Error> {
    let model = target.model;
    let (mut contents, system) = contents(&request.messages, model)?;
    if gemini.quirks.thought_replay == ThoughtReplay::CurrentTurn {
        drop_finished_signatures(&mut contents);
    }
    let mut options = request
        .options
        .get(KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut tools = Vec::new();
    if !request.tools.is_empty() {
        let declared: Vec<Value> = request.tools.iter().map(declaration).collect();
        tools.push(json!({ "functionDeclarations": declared }));
    }
    if let Some(Value::Array(hosted)) = options.remove("tools") {
        tools.extend(hosted);
    }

    let mut body = Map::new();
    body.insert("contents".into(), Value::Array(contents));
    if !system.is_empty() {
        body.insert("systemInstruction".into(), json!({ "parts": system }));
    }
    let config = generation_config(model, request);
    if !config.is_empty() {
        body.insert("generationConfig".into(), Value::Object(config));
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if request.tool_choice != ToolChoice::Auto {
        body.insert("toolConfig".into(), tool_config(&request.tool_choice));
    }
    let mut body = Value::Object(body);
    crate::protocol::merge(&mut body, &options);
    Ok(body)
}
