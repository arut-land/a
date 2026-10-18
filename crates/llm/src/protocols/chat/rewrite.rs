//! The writes made to a finished body, after the request's own options
//! merged into it: the output cap's spelling, the usage chunk, and each
//! host's own rewrite.

use serde_json::{Map, Value, json};

use super::{OutputCap, Quirks, Rewrite};
use crate::error::Error;
use crate::json::Lenient;
use crate::protocols::openai;

/// The writes made after the caller's options merged, which read them: the
/// output cap's spelling, the usage chunk, and the host's own rewrite.
pub(super) fn finish(quirks: &Quirks, body: &mut Map<String, Value>) -> Result<(), Error> {
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
    rewrite(quirks.rewrite, body)
}

/// The body's messages, each as its object.
fn messages_mut(body: &mut Map<String, Value>) -> impl Iterator<Item = &mut Map<String, Value>> {
    body.get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
}

/// The host's rewrite of the finished body: a refusal where the host would
/// answer with an error or silently do something else, and each host's
/// spellings and content shapes.
fn rewrite(kind: Rewrite, body: &mut Map<String, Value>) -> Result<(), Error> {
    let forced = body
        .get("tool_choice")
        .and_then(|choice| choice.at("/function/name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    match kind {
        Rewrite::LlamaCpp => {
            if let Some(name) = forced {
                return Err(Error::unsupported(format!(
                    "llama.cpp cannot force a specific tool: `llama-server` takes only `auto`, \
                     `none` or `required` and treats anything else as `auto`, so asking for \
                     `{name}` would return whichever tool the model picked; require a call, or \
                     offer only `{name}`"
                )));
            }
        }
        Rewrite::Moonshot => {
            if forced.is_some() {
                return Err(Error::unsupported("Moonshot cannot force a specific tool"));
            }
            if body.get("tool_choice").and_then(Value::as_str) == Some("required") {
                body.insert("tool_choice".to_owned(), Value::from("auto"));
                if let Some(Value::Array(messages)) = body.get_mut("messages") {
                    messages.push(json!({"role": "user",
                        "content": "Please select a tool to handle the current issue."}));
                }
            }
        }
        // Mira takes every content as one string. Perplexity spends output
        // on a text-part array (a short cap comes back empty at `length`),
        // so text-only arrays go as one string.
        Rewrite::Perplexity | Rewrite::Mira => {
            let all = kind == Rewrite::Mira;
            for content in messages_mut(body).filter_map(|message| message.get_mut("content")) {
                if let Value::Array(parts) = content
                    && (all || parts.iter().all(|part| part.str("type") == Some("text")))
                {
                    let texts: Vec<&str> =
                        parts.iter().filter_map(|part| part.str("text")).collect();
                    *content = Value::String(texts.join("\n"));
                }
            }
        }
        Rewrite::DeepSeek => deepseek(body),
        Rewrite::Mistral => mistral(body),
        Rewrite::Ollama => {
            if let Some(key) = ["num_ctx", "options"]
                .into_iter()
                .find(|key| body.contains_key(*key))
            {
                return Err(Error::unsupported(format!(
                    "Ollama's OpenAI-compatible API ignores `{key}`; only its native API reads it"
                )));
            }
        }
        Rewrite::None | Rewrite::OpenRouter => {}
    }
    Ok(())
}

/// `DeepSeek` rejects a forced tool choice while thinking, and every model
/// but `deepseek-chat` thinks unless `thinking` says `disabled`, so the
/// choice is relaxed to the default there.
fn deepseek(body: &mut Map<String, Value>) {
    let thinking = match body
        .get("thinking")
        .and_then(|thinking| thinking.str("type"))
    {
        Some(mode) => !mode.eq_ignore_ascii_case("disabled"),
        None => body.get("model").and_then(Value::as_str) != Some("deepseek-chat"),
    };
    let forced = body
        .get("tool_choice")
        .is_some_and(|choice| choice.is_object() || choice.as_str() == Some("required"));
    if thinking && forced {
        body.remove("tool_choice");
    }
}

/// Mistral rejects a forced tool call beside a JSON format, so the choice
/// is relaxed rather than the format dropped; and its content chunks.
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
    for content in messages_mut(body).filter_map(|message| message.get_mut("content")) {
        for part in content.as_array_mut().into_iter().flatten() {
            *part = mistral_chunk(part);
        }
    }
}

/// One content part as the Mistral chunk that carries it: a file's data is
/// a `document_url` and its id a `file` chunk, audio is its base64 string,
/// and an image keeps only its `image_url`, for every chunk forbids unknown
/// keys. Any other part stays as it is.
fn mistral_chunk(part: &Value) -> Value {
    let field = |pointer: &str| part.at(pointer).and_then(Value::as_str);
    match part.str("type") {
        Some("image_url") => json!({"type": "image_url", "image_url": part.get("image_url")}),
        Some("input_audio") => {
            json!({"type": "input_audio", "input_audio": field("/input_audio/data")})
        }
        Some("file") => match field("/file/file_data") {
            Some(data) => json!({"type": "document_url", "document_url": data,
                "document_name": field("/file/filename")}),
            None => json!({"type": "file", "file_id": field("/file/file_id")}),
        },
        _ => part.clone(),
    }
}
