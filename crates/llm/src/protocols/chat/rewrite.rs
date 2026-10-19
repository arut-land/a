//! The writes made to a finished body after the request's own options
//! merged into it, since they read them: the output cap's spelling, the
//! usage chunk, and each host's own rewrite.

use serde_json::{Map, Value, json};

use super::encode::{Role, joined_text};
use super::{OutputCap, PartKind, Quirks, Vendor};
use crate::conversation::History;
use crate::error::Error;
use crate::json::Lenient;
use crate::models::openai;

/// `history` is the conversation, when the body leaves it to be spliced;
/// its messages took their host's shapes as they were encoded
/// ([`message`]).
pub(super) fn finish(
    quirks: &Quirks,
    body: &mut Map<String, Value>,
    history: Option<&mut History>,
) -> Result<(), Error> {
    // A `vendor/model` id (a proxy behind the base URL) keeps `max_tokens`.
    let reasoning = body
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| !model.contains('/') && openai::reasons(model) == Some(true));
    if quirks.output_cap == OutputCap::OpenAiReasoning
        && reasoning
        && let Some(max) = body.remove("max_tokens")
    {
        body.entry("max_completion_tokens").or_insert(max);
    }
    // The caller's own `stream_options` stand.
    if quirks.stream_usage
        && let Some(options) = body
            .entry("stream_options")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
    {
        options.entry("include_usage").or_insert(Value::Bool(true));
    }
    if history.is_none() {
        let messages = body.get_mut("messages").and_then(Value::as_array_mut);
        for message in messages.into_iter().flatten() {
            self::message(quirks.vendor, message);
        }
    }
    rewrite(quirks.vendor, body, history)
}

/// One message's content as the host takes it. Mira takes every content as
/// one string. Perplexity spends output on a text-part array (a short cap
/// comes back empty at `length`), so text-only arrays go as one string.
/// Mistral's parts go as its chunks.
pub(super) fn message(vendor: Vendor, message: &mut Value) {
    let Some(Value::Array(parts)) = message.get_mut("content") else {
        return;
    };
    match vendor {
        Vendor::Perplexity | Vendor::Mira
            if vendor == Vendor::Mira
                || parts
                    .iter()
                    .all(|part| PartKind::of(part) == Some(PartKind::Text)) =>
        {
            message["content"] = Value::String(joined_text(parts));
        }
        Vendor::Mistral => {
            for part in parts {
                *part = mistral_chunk(part);
            }
        }
        _ => {}
    }
}

/// The host's rewrite of the finished body: a refusal where the host would
/// answer with an error or silently do something else, and each host's
/// spellings and content shapes.
fn rewrite(
    vendor: Vendor,
    body: &mut Map<String, Value>,
    history: Option<&mut History>,
) -> Result<(), Error> {
    let choice = body.get("tool_choice");
    let forced = choice.and_then(|choice| choice.at("/function/name")?.as_str());
    match vendor {
        Vendor::LlamaCpp => {
            if let Some(name) = forced {
                return Err(Error::unsupported(format!(
                    "llama.cpp cannot force a specific tool: `llama-server` takes only `auto`, \
                     `none` or `required` and treats anything else as `auto`, so asking for \
                     `{name}` would return whichever tool the model picked; require a call, or \
                     offer only `{name}`"
                )));
            }
        }
        Vendor::Moonshot => {
            if forced.is_some() {
                return Err(Error::unsupported("Moonshot cannot force a specific tool"));
            }
            if choice.and_then(Value::as_str) == Some("required") {
                body.insert("tool_choice".to_owned(), Value::from("auto"));
                let steer = "Please select a tool to handle the current issue.";
                let steer = Role::User.message(steer.into());
                if let Some(history) = history {
                    history.tail.push(steer);
                } else if let Some(Value::Array(messages)) = body.get_mut("messages") {
                    messages.push(steer);
                }
            }
        }
        // DeepSeek rejects a forced tool choice while thinking, and every
        // model but `deepseek-chat` thinks unless `thinking` says
        // `disabled`, so the choice is relaxed to the default there.
        Vendor::DeepSeek => {
            let thinking = match body
                .get("thinking")
                .and_then(|thinking| thinking.str("type"))
            {
                Some(mode) => !mode.eq_ignore_ascii_case("disabled"),
                None => body.get("model").and_then(Value::as_str) != Some("deepseek-chat"),
            };
            if thinking && choice.is_some_and(|c| c.is_object() || c.as_str() == Some("required")) {
                body.remove("tool_choice");
            }
        }
        Vendor::Mistral => mistral(body),
        Vendor::Ollama => {
            if let Some(key) = ["num_ctx", "options"]
                .into_iter()
                .find(|key| body.contains_key(*key))
            {
                return Err(Error::unsupported(format!(
                    "Ollama's OpenAI-compatible API ignores `{key}`; only its native API reads it"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Mistral rejects a forced tool call beside a JSON format, so the choice
/// is relaxed rather than the format dropped.
fn mistral(body: &mut Map<String, Value>) {
    let forces = body
        .get("tool_choice")
        .is_some_and(|choice| !matches!(choice.as_str(), Some("auto" | "none")));
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let structured = body
        .get("response_format")
        .and_then(|format| format.str("type"))
        .is_some_and(|kind| matches!(kind, "json_schema" | "json_object"));
    if forces && has_tools && structured {
        body.insert("tool_choice".to_owned(), Value::from("auto"));
    }
}

/// One content part as the Mistral chunk that carries it: a file's data is
/// a `document_url` and its id a `file` chunk, audio is its base64 string,
/// and an image keeps only its `image_url`, for every chunk forbids unknown
/// keys. Any other part stays as it is.
fn mistral_chunk(part: &Value) -> Value {
    let field = |pointer: &str| part.at(pointer).and_then(Value::as_str);
    match PartKind::of(part) {
        Some(PartKind::ImageUrl) => {
            json!({"type": "image_url", "image_url": part.get("image_url")})
        }
        Some(PartKind::InputAudio) => {
            json!({"type": "input_audio", "input_audio": field("/input_audio/data")})
        }
        Some(PartKind::File) => match field("/file/file_data") {
            Some(data) => json!({"type": "document_url", "document_url": data,
                "document_name": field("/file/filename")}),
            None => json!({"type": "file", "file_id": field("/file/file_id")}),
        },
        _ => part.clone(),
    }
}
