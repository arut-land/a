//! A request as a `GenerateContent` body. System text goes in the
//! `systemInstruction`, the rest in `contents`; parts this protocol produced
//! go back as Gemini sent them, and everything else is rebuilt.
//!
//! Options a model cannot take are left out rather than failing the
//! request: an effort a model has no level for, a budget out of its range,
//! turning off thinking a model cannot turn off, more than 5 stop
//! sequences, a seed beyond 32 bits, and `parallel_tool_calls: false`.

use std::num::NonZeroU32;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{Gemini, KEY, ThoughtReplay};
use crate::conversation::{self, Fold, History, Settled};
use crate::error::Error;
use crate::json::{Lenient, obj};
use crate::models::Reasoning;
use crate::models::google as model;
use crate::protocol::Target;
use crate::replay::{Replay, replay, replays};
use crate::spec::{
    AssistantPart, Effort, Format, Generation, Media, Message, Output, Request, SmolStr, Source,
    Tool, ToolChoice, ToolResult, UserPart,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Model,
}

impl Role {
    const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Model => "model",
        }
    }
}

/// `functionCallingConfig.mode`.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum Mode {
    Auto,
    None,
    Any,
}

/// `object` without its `null` fields: the options a request leaves unset.
fn set(mut object: Value) -> Value {
    if let Some(fields) = object.as_object_mut() {
        fields.retain(|_, value| !value.is_null());
    }
    object
}

/// `value` unless it is an empty object or list.
fn some(value: Value) -> Option<Value> {
    (value != json!({}) && value != json!([])).then_some(value)
}

fn text_part(text: &str) -> Value {
    obj!({"text": text, "thought": false})
}

/// `media` as part data, a URL by reference and data inline; as a `whole`
/// part, marked as no thought.
fn media(media: &Media, whole: bool) -> Result<Value, Error> {
    let mime = media.media_type.as_str();
    let mut part = match &media.source {
        Source::Data { data } => {
            obj!({"inlineData": obj!({"mimeType": mime, "data": data.as_str()})})
        }
        Source::Url { url } => {
            obj!({"fileData": obj!({"mimeType": mime, "fileUri": url.as_str()})})
        }
        Source::File { .. } => {
            let why = "Gemini cannot carry media by a file id; send its URI as a URL";
            return Err(Error::unsupported(why));
        }
    };
    if whole {
        part["thought"] = false.into();
    }
    Ok(part)
}

fn is_response(part: &Value) -> bool {
    part.get("functionResponse").is_some()
}

/// What a conversation's contents read besides its messages.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Cx {
    model: SmolStr,
    thought_replay: ThoughtReplay,
}

/// The contents of a conversation, a message at a time, and its system
/// text. Adjacent user messages are one content, but function responses
/// and other user parts go in contents of their own: Gemini answers text
/// that shares a content with responses poorly.
#[derive(Clone)]
pub(super) struct Contents {
    cx: Cx,
    /// The contents not settled: one a user message may still join, and,
    /// where only the current turn keeps its signatures, the turn since the
    /// newest user text.
    open: Vec<(Role, Vec<Value>)>,
    /// Gemini takes system text only in its instruction, so a later system
    /// message joins the leading one.
    system: Vec<Value>,
}

impl Contents {
    pub(super) fn new(model: &str, thought_replay: ThoughtReplay) -> Self {
        Self {
            cx: Cx {
                model: model.into(),
                thought_replay,
            },
            open: Vec::new(),
            system: Vec::new(),
        }
    }

    fn add(&mut self, role: Role, parts: Vec<Value>) {
        // Gemini rejects a content with no parts.
        if parts.is_empty() {
            return;
        }
        if role == Role::User
            && let Some((Role::User, held)) = self.open.last_mut()
            && held.last().map(is_response) == parts.first().map(is_response)
        {
            held.extend(parts);
        } else {
            self.open.push((role, parts));
        }
    }

    /// The contents not settled, and the system instruction's parts.
    pub(super) fn finish(self) -> (Vec<Value>, Vec<Value>) {
        let mut contents: Vec<Value> = self.open.into_iter().map(content).collect();
        if self.cx.thought_replay == ThoughtReplay::CurrentTurn {
            drop_finished_signatures(&mut contents);
        }
        (contents, self.system)
    }
}

fn content((role, parts): (Role, Vec<Value>)) -> Value {
    obj!({"parts": parts, "role": role.as_str()})
}

/// Whether a content is a user's text, which starts a turn.
fn user_text((role, parts): &(Role, Vec<Value>)) -> bool {
    *role == Role::User
        && parts.iter().any(|part| part.get("text").is_some())
        && !parts.iter().any(is_response)
}

impl Fold for Contents {
    type Cx = Cx;

    fn cx(&self) -> &Cx {
        &self.cx
    }

    /// Every content but the last when that is a user's, which the next may
    /// join; where only the current turn keeps its signatures, those before
    /// the newest user text, without theirs.
    fn settle(&mut self, out: &mut Settled) {
        let settled = match self.cx.thought_replay {
            ThoughtReplay::All => {
                let user = self
                    .open
                    .last()
                    .is_some_and(|(role, _)| *role == Role::User);
                self.open.len() - usize::from(user)
            }
            ThoughtReplay::CurrentTurn => self.open.iter().rposition(user_text).unwrap_or(0),
        };
        let mut settled: Vec<Value> = self.open.drain(..settled).map(content).collect();
        if self.cx.thought_replay == ThoughtReplay::CurrentTurn {
            drop_signatures(&mut settled);
        }
        for content in &settled {
            out.item(content);
        }
    }

    fn push(&mut self, _: usize, message: &Message) -> Result<(), Error> {
        let model = &*self.cx.model;
        let ids = model::takes_call_ids(model);
        match message {
            Message::System(text) => {
                let text = text.text();
                if !text.trim().is_empty() {
                    self.system.push(text_part(&text));
                }
            }
            Message::User(user) => {
                let parts = user.content.iter().filter_map(|part| match part {
                    // Blank text says nothing, and Gemini rejects it.
                    UserPart::Text(text) if text.text.trim().is_empty() => None,
                    UserPart::Text(text) => Some(Ok(text_part(&text.text))),
                    UserPart::Media(part) => Some(media(part, true)),
                });
                let parts = parts.collect::<Result<_, _>>()?;
                self.add(Role::User, parts);
            }
            Message::Tool(tool) => {
                let mut moved = vec![text_part(RESULT_IMAGES)];
                let parts = tool.results.iter();
                let parts = parts.map(|result| function_response(result, model, ids, &mut moved));
                let parts = parts.collect::<Result<_, _>>()?;
                self.add(Role::User, parts);
                if moved.len() > 1 {
                    self.add(Role::User, moved);
                }
            }
            Message::Assistant(assistant) => {
                let parts = assistant.content.iter();
                let parts = parts.filter_map(|part| assistant_part(part, model, ids).transpose());
                let parts = parts.collect::<Result<_, _>>()?;
                self.add(Role::Model, parts);
            }
        }
        Ok(())
    }
}

/// The contents and system instruction parts of `messages` for `model`.
#[cfg(test)]
pub(super) fn contents(
    messages: &[Message],
    model: &str,
) -> Result<(Vec<Value>, Vec<Value>), Error> {
    let mut contents = Contents::new(model, ThoughtReplay::All);
    for (at, message) in messages.iter().enumerate() {
        contents.push(at, message)?;
    }
    Ok(contents.finish())
}

/// A tool result as a function response. Its error says so under the
/// `error` key, as Gemini's SDKs spell a failed function. Media it may not
/// hold inline (JPEG, PNG or WEBP data) moves to `moved`, for a user
/// content after the responses.
fn function_response(
    result: &ToolResult,
    model: &str,
    ids: bool,
    moved: &mut Vec<Value>,
) -> Result<Value, Error> {
    let images = model::reads_result_images(model);
    let inline_image = |part: &Media| {
        let image = matches!(&*part.media_type, "image/jpeg" | "image/png" | "image/webp");
        image && matches!(part.source, Source::Data { .. })
    };
    let (mut values, mut inline) = (Vec::new(), Vec::new());
    match &result.output {
        Output::Text(text) => values.push(text.as_str().into()),
        // Gemini reads a result as a value, which goes into the contents
        // tree a request edits (its signatures dropped): the one place an
        // output is read back into a tree.
        Output::Json(value) => values.push(serde_json::from_str(value.raw()).unwrap_or_default()),
        Output::Content(parts) => {
            for part in parts {
                match part {
                    UserPart::Text(text) => values.push(text.text.as_str().into()),
                    UserPart::Media(part) if images && inline_image(part) => {
                        inline.push(media(part, false)?);
                    }
                    UserPart::Media(part) => moved.push(media(part, true)?),
                }
            }
        }
    }
    let blank = |value: &Value| value.as_str().is_some_and(|text| text.trim().is_empty());
    if values.iter().all(blank) && inline.is_empty() {
        let error = if result.error { "[tool error] " } else { "" };
        values = vec![format!("{error}{NO_OUTPUT}").into()];
    }
    // Gemini 2 rejects a result of several parts: its texts join.
    if !images && values.len() > 1 {
        let text = |value: &Value| match value {
            Value::String(text) => text.clone(),
            value => value.to_string(),
        };
        let texts: Vec<String> = values.iter().map(text).collect();
        values = vec![texts.join("\n").into()];
    }
    let key = if result.error { "error" } else { "result" };
    let value = match values.len() {
        0 | 1 => values.pop(),
        _ => Some(Value::Array(values)),
    };
    let response = set(obj!({
        "name": result.name.as_str(),
        "id": ids.then_some(result.id.as_str()),
        "response": value.map(|value| obj!({ key: value })),
        "parts": some(inline.into()),
    }));
    Ok(obj!({"functionResponse": response, "thought": false}))
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
        let item = crate::replay::item(&opaque.meta, KEY);
        return Ok(item.filter(|_| replays(part, KEY)));
    }
    if let Replay::Item(mut item) = replay(part, KEY, |_| Map::new()) {
        if let AssistantPart::ToolCall(call) = part
            && let Some(function) = item.get_mut("functionCall").and_then(Value::as_object_mut)
        {
            // The call takes the id its response answers.
            if ids {
                function.insert("id".into(), call.id.as_str().into());
            } else {
                function.remove("id");
            }
        }
        // Gemini rejects the whole request over a signature that is not
        // base64.
        if item.str("thoughtSignature").is_some_and(|s| !is_base64(s))
            && let Some(fields) = item.as_object_mut()
        {
            fields.remove("thoughtSignature");
        }
        return Ok(Some(item));
    }
    Ok(Some(match part {
        AssistantPart::Text(text) if !text.text.trim().is_empty() => {
            obj!({"text": text.text.as_str()})
        }
        AssistantPart::Reasoning(thought)
            if !thought.redacted && !thought.text.trim().is_empty() =>
        {
            obj!({"thought": true, "text": thought.text.as_str()})
        }
        AssistantPart::ToolCall(call) => {
            let (id, name) = (ids.then_some(call.id.as_str()), call.name.as_str());
            // As a result is: a tree, in the contents a request edits.
            let args: Value = serde_json::from_str(call.input.raw()).unwrap_or_default();
            let function = set(obj!({"name": name, "args": args, "id": id}));
            let signature = model::validates_signatures(model).then_some(UNSIGNED_CALL);
            set(obj!({"functionCall": function, "thoughtSignature": signature}))
        }
        AssistantPart::Media(part) => media(part, true)?,
        _ => return Ok(None),
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
    drop_signatures(&mut contents[..current.unwrap_or(0)]);
}

/// Drop the thought signatures of every part of `contents`.
fn drop_signatures(contents: &mut [Value]) {
    for content in contents {
        let parts = content.get_mut("parts").and_then(Value::as_array_mut);
        for part in parts.into_iter().flatten().filter_map(Value::as_object_mut) {
            part.remove("thoughtSignature");
        }
    }
}

fn tool_config(choice: &ToolChoice) -> Value {
    let (mode, names) = match choice {
        ToolChoice::Auto => (Mode::Auto, None),
        ToolChoice::None => (Mode::None, None),
        ToolChoice::Required => (Mode::Any, None),
        ToolChoice::Tool(name) => (Mode::Any, Some([name])),
    };
    let config = set(json!({"mode": mode, "allowed_function_names": names}));
    obj!({"functionCallingConfig": config})
}

/// A tool as its function declaration. Its JSON Schema goes whole as
/// `parametersJsonSchema`, which reads full JSON Schema.
fn declaration(tool: &Tool) -> Value {
    let description = (!tool.description.is_empty()).then_some(&tool.description);
    let schema = &tool.schema;
    set(json!({"name": tool.name, "description": description, "parametersJsonSchema": schema}))
}

/// The `thinkingConfig` `generation` asks of `model`, with thought
/// summaries whenever thinking stays on, so reasoning streams.
fn thinking_config(model: &str, generation: &Generation) -> Value {
    let effort = generation.reasoning;
    let budget = generation.reasoning_budget.map(NonZeroU32::get);
    let (mut level, mut tokens, mut summaries) = (None, None, false);
    match model::reasoning(model) {
        Reasoning::None => return Value::Null,
        Reasoning::Unknown => match effort {
            Some(Effort::None) => {}
            // Gemini's levels stop at `high`.
            Some(effort @ (Effort::Minimal | Effort::Low | Effort::Medium | Effort::High)) => {
                level = Some(effort);
            }
            _ => tokens = budget,
        },
        Reasoning::Takes(thinking) => {
            summaries = thinking.budget.is_some() || !thinking.efforts.is_empty();
            match (effort, thinking.budget) {
                // Off is a zero budget, or level `minimal`, with no summaries.
                (Some(Effort::None), range) if thinking.can_disable => {
                    tokens = range.map(|_| 0);
                    level = range.is_none().then_some(Effort::Minimal);
                    summaries = false;
                }
                // Thinking it cannot turn off goes on as the model's default.
                (Some(Effort::None), _) => {}
                // Gemini 2.5 takes a budget, and no table turns an effort
                // into one.
                (_, Some(range)) => tokens = budget.filter(|budget| range.contains(*budget)),
                (Some(effort), None) if thinking.takes(effort) => level = Some(effort),
                _ => {}
            }
        }
    }
    let summaries = summaries.then_some(true);
    set(json!({"thinkingLevel": level, "thinkingBudget": tokens, "includeThoughts": summaries}))
}

fn generation_config(model: &str, request: &Request) -> Value {
    let generation = &request.generation;
    let schema = match &request.format {
        Some(Format::Json { schema, .. }) => Some(schema),
        _ => None,
    };
    let stop = &generation.stop;
    set(json!({
        "responseMimeType": schema.map(|_| "application/json"),
        "responseJsonSchema": schema,
        "temperature": generation.temperature,
        "maxOutputTokens": generation.max_output_tokens,
        "topP": generation.top_p,
        "topK": generation.top_k,
        "seed": generation.seed.and_then(|seed| i32::try_from(seed).ok()),
        "stopSequences": (1..=5).contains(&stop.len()).then_some(stop),
        "thinkingConfig": some(thinking_config(model, generation)),
    }))
}

/// The request body for `request` to `target`. This protocol's options are
/// body fields merged over it last: their `tools` join the declared ones,
/// and objects such as `generationConfig` merge key by key.
pub(crate) fn body(
    gemini: &Gemini,
    target: &Target<'_>,
    request: &Request,
) -> Result<(Value, Option<History>), Error> {
    let model = target.model;
    let contents = Contents::new(model, gemini.quirks.thought_replay);
    let (contents, settled) = conversation::fold(&request.messages, target, contents)?;
    let (tail, system) = contents.finish();
    let mut options = request.options_for(KEY);
    let declared = some(
        (request.tools.iter())
            .map(|tool| tool.encoded("gemini", declaration))
            .collect(),
    );
    let mut tools = Vec::from_iter(declared.map(|tools| obj!({"functionDeclarations": tools})));
    if let Some(Value::Array(hosted)) = options.remove("tools") {
        tools.extend(hosted);
    }
    let choice = &request.tool_choice;
    let history = (!options.contains_key("contents")).then(|| History {
        key: "contents",
        head: Vec::new(),
        settled,
        tail,
    });
    let mut body = set(obj!({
        "systemInstruction": some(system.into()).map(|parts| obj!({"parts": parts})),
        "generationConfig": some(generation_config(model, request)),
        "tools": some(tools.into()),
        "toolConfig": (*choice != ToolChoice::Auto).then(|| tool_config(choice)),
    }));
    crate::json::overlay(&mut body, &options);
    Ok((body, history))
}
