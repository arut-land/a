//! `OpenAI` Chat Completions (`POST /chat/completions`), and the
//! OpenAI-compatible hosts that serve it with their quirks.

mod decode;
mod encode;
mod hosts;
mod merge;
mod rewrite;
mod rules;

pub use hosts::HOSTS;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::schema::{legal_call_id, short_hash};
use crate::error::Error;
use crate::protocol::{Decoder, Encoded, Host, Hosted, Protocol, Target};
use crate::spec::{Reason, Request};

/// Where this protocol's options and part state go.
pub const KEY: &str = "openai.chat";

/// How a host addresses a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Routing {
    /// The path under the base URL, the model in the body.
    Path,
    /// Azure: the model is a deployment in the URL
    /// (`/openai/deployments/{model}{path}?api-version={api_version}`), and
    /// the body names none.
    Deployment { api_version: &'static str },
}

/// How a host spells the output-token cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputCap {
    /// `max_tokens`, which every compatible server takes.
    Legacy,
    /// `max_completion_tokens` for `OpenAI`'s reasoning models, which refuse
    /// `max_tokens`; any other model keeps the legacy field.
    OpenAiReasoning,
}

/// Where a host takes the system messages that come after the
/// conversation begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaterSystem {
    /// Where the conversation has them.
    InPlace,
    /// Joined with the leading ones into one system message.
    Leading,
}

/// Whose model rules, option vocabulary, media support and body rewrite
/// (`rewrite.rs`) a host follows. An option the model cannot take is left
/// out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    OpenAi,
    Azure,
    OpenRouter,
    DeepSeek,
    Mistral,
    Groq,
    Xai,
    Together,
    Venice,
    Moonshot,
    Zai,
    LlamaCpp,
    Ollama,
    Cohere,
    Perplexity,
    MiniMax,
    Mimo,
    Mira,
    /// A gateway with no known option mapping: typed options beyond the
    /// sampling ones are not sent; the request's own options carry them.
    Other,
}

/// The fields an assistant message carries reasoning text under, in the
/// order they are read: servers that send several send the same text under
/// each, so the first that carries text is the one kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasoningField {
    ReasoningContent,
    Reasoning,
    ReasoningText,
}

impl ReasoningField {
    pub const ALL: [Self; 3] = [Self::ReasoningContent, Self::Reasoning, Self::ReasoningText];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReasoningContent => "reasoning_content",
            Self::Reasoning => "reasoning",
            Self::ReasoningText => "reasoning_text",
        }
    }

    /// The first field of `texts` (one per field, in [`Self::ALL`]'s order)
    /// that carries text, with it.
    pub fn first(texts: [Option<&str>; 3]) -> Option<(Self, &str)> {
        Self::ALL
            .into_iter()
            .zip(texts)
            .find_map(|(field, text)| Some((field, text.filter(|text| !text.is_empty())?)))
    }
}

/// The content-part types this wire reads and writes. A part is
/// `{"type": kind, kind: payload}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PartKind {
    Text,
    Refusal,
    Thinking,
    ImageUrl,
    InputAudio,
    VideoUrl,
    File,
}

impl PartKind {
    /// The kind `part`'s `type` names, if this wire knows it.
    fn of(part: &Value) -> Option<Self> {
        Self::deserialize(part.get("type")?).ok()
    }

    /// A part of this kind carrying `payload`.
    fn part(self, payload: Value) -> Value {
        let kind = json!(self);
        let key = kind.as_str().unwrap_or_default().to_owned();
        Value::Object(crate::json::object([
            ("type".to_owned(), kind),
            (key, payload),
        ]))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Quirks {
    /// The endpoint path, appended to the base URL.
    pub path: &'static str,
    pub routing: Routing,
    pub vendor: Vendor,
    /// Whether `tools` and `tool_choice` reach the host. A host without
    /// them gets the conversation's calls and results as text.
    pub supports_tools: bool,
    /// Whether a JSON format maps to `response_format`.
    pub supports_response_format: bool,
    /// Whether `response_format` goes beside tools before any tool result;
    /// otherwise it waits for one, so it does not suppress the calls.
    pub response_format_with_tools: bool,
    /// Whether an image inside a `tool` message is read. Otherwise it moves
    /// to a user message after the results.
    pub image_tool_results: bool,
    /// Whether a tool result's content is always sent as parts.
    pub tool_result_parts: bool,
    /// Whether tools that do not say are sent strict.
    pub strict_tools: bool,
    pub later_system: LaterSystem,
    /// Whether the request asks for the usage chunk (`stream_options`).
    pub stream_usage: bool,
    /// Whether `[DONE]` ends a turn no finish reason ended: as tool calls
    /// when it holds one, a stop otherwise. Without it such a stream is
    /// incomplete; with it, so is a cut-off stream that still sends
    /// `[DONE]`, so set it only for a gateway known to omit the field.
    pub done_without_finish_reason: bool,
    pub output_cap: OutputCap,
    /// Whether an upstream's own `native_finish_reason` stands in for a
    /// missing normalized one (a gateway's).
    pub native_finish_reason: bool,
    /// Finish reasons the host documents beyond the shared ones.
    pub finishes: &'static [(&'static str, Reason)],
    /// The field rebuilt reasoning goes back under, which every assistant
    /// message then carries, empty when the turn has none.
    pub reasoning_field: Option<ReasoningField>,
    /// Whether `completion_tokens_details.reasoning_tokens` is a part of
    /// `completion_tokens`, as `OpenAI` documents; otherwise unreported.
    pub reliable_reasoning_count: bool,
    /// Whether a bare JSON string is the whole text of a reply.
    pub bare_string_reply: bool,
    /// Whether a file may go by its provider file id.
    pub file_ids: bool,
    /// The length foreign tool-call ids are cut to.
    pub call_id_limit: Option<usize>,
}

impl Quirks {
    /// The baseline every compatible host starts from.
    pub const OPENAI: Self = Self {
        path: "/chat/completions",
        routing: Routing::Path,
        vendor: Vendor::Other,
        supports_tools: true,
        supports_response_format: true,
        response_format_with_tools: false,
        image_tool_results: false,
        tool_result_parts: false,
        strict_tools: false,
        later_system: LaterSystem::InPlace,
        stream_usage: true,
        done_without_finish_reason: false,
        output_cap: OutputCap::Legacy,
        native_finish_reason: false,
        finishes: &[],
        reasoning_field: None,
        reliable_reasoning_count: true,
        bare_string_reply: false,
        file_ids: true,
        call_id_limit: None,
    };
}

impl Default for Quirks {
    fn default() -> Self {
        Self::OPENAI
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Chat {
    pub quirks: Quirks,
}

impl Chat {
    pub const fn new(quirks: Quirks) -> Self {
        Self { quirks }
    }
}

impl Hosted for Chat {
    type Quirks = Quirks;

    const KEY: &'static str = KEY;
    const HOSTS: &'static [(Host, Quirks)] = HOSTS;

    fn with(quirks: Quirks) -> Self {
        Self::new(quirks)
    }
}

impl Protocol for Chat {
    fn key(&self) -> &'static str {
        KEY
    }

    fn encode(&self, target: &Target<'_>, request: &Request) -> Result<Encoded, Error> {
        let path = match self.quirks.routing {
            Routing::Path => self.quirks.path.to_owned(),
            Routing::Deployment { api_version } => format!(
                "/openai/deployments/{}{}?api-version={api_version}",
                target.model.trim_start_matches('/'),
                self.quirks.path,
            ),
        };
        Ok(Encoded {
            path,
            body: encode::body(&self.quirks, target, request)?,
            headers: Vec::new(),
        })
    }

    fn decoder(&self, _: &Target<'_>, _: &Request) -> Box<dyn Decoder> {
        Box::new(decode::ChatDecoder::new(self.quirks))
    }

    /// A Responses `call|item` id joins its legal halves with `_`, hashed
    /// past 40 characters; a host with a limit cuts any other id to it, and
    /// the rest keep theirs.
    fn tool_id(&self, id: &str) -> Option<String> {
        let fitted = match id.split_once('|') {
            Some((call, item)) => {
                let call = legal_call_id(call, usize::MAX);
                let item = legal_call_id(item, usize::MAX);
                let joined = if item.is_empty() {
                    call.clone()
                } else {
                    format!("{call}_{item}")
                };
                if joined.len() <= 40 {
                    joined
                } else {
                    let hash: String = short_hash(id).chars().take(8).collect();
                    let prefix: String = call.chars().take(39 - hash.len()).collect();
                    format!("{prefix}_{hash}")
                }
            }
            None => id.chars().take(self.quirks.call_id_limit?).collect(),
        };
        (fitted != id).then_some(fitted)
    }
}

#[cfg(test)]
mod tests;
