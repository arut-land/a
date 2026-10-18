//! `OpenAI` Chat Completions (`POST /chat/completions`), and the
//! OpenAI-compatible hosts that serve it with their quirks (ported from
//! rig's Chat wire and its dialects).

mod decode;
mod encode;
mod hosts;
mod merge;
mod rewrite;
mod rules;

pub use hosts::HOSTS;

use crate::error::Error;
use crate::protocol::{Decoder, Encoded, Framing, Host, Hosted, Protocol, Target};
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

/// The host's own change to the finished body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rewrite {
    None,
    /// A forced tool choice dropped while the model thinks.
    DeepSeek,
    /// Every content array flattened to a string.
    Mira,
    /// Text-only content arrays sent as one string.
    Perplexity,
    /// A forced tool choice relaxed to `auto` beside a structured format,
    /// and content parts as Mistral's chunks.
    Mistral,
    /// A specific-function tool choice refused: `llama-server` treats it as
    /// `auto`.
    LlamaCpp,
    /// A specific-function tool choice refused, and `required` coerced to
    /// `auto` with a steering message.
    Moonshot,
    /// Model ids name the upstream vendor; a PDF may go by URL.
    OpenRouter,
    /// `num_ctx` and `options` refused: only Ollama's native API reads them.
    Ollama,
}

/// Whose model rules and option vocabulary a host follows: which models
/// read images, and how the generation options are spelled. An option the
/// model cannot take is left out.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Quirks {
    /// The endpoint path, appended to the base URL.
    pub path: &'static str,
    pub routing: Routing,
    pub vendor: Vendor,
    pub rewrite: Rewrite,
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
    /// incomplete.
    pub done_without_finish_reason: bool,
    pub output_cap: OutputCap,
    /// Whether an upstream's own `native_finish_reason` stands in for a
    /// missing normalized one (a gateway's).
    pub native_finish_reason: bool,
    /// Finish reasons the host documents beyond the shared ones.
    pub finishes: &'static [(&'static str, Reason)],
    /// The field rebuilt reasoning goes back under, which every assistant
    /// message then carries, empty when the turn has none.
    pub reasoning_field: Option<&'static str>,
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
        rewrite: Rewrite::None,
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

    /// These quirks without the streamed usage chunk.
    #[must_use]
    pub const fn without_stream_usage(mut self) -> Self {
        self.stream_usage = false;
        self
    }

    /// These quirks for a gateway whose streams omit `finish_reason`. A
    /// cut-off stream that still sends `[DONE]` then reads as complete, so
    /// set it only for a gateway known to omit the field.
    #[must_use]
    pub const fn done_without_finish_reason(mut self) -> Self {
        self.done_without_finish_reason = true;
        self
    }

    /// These quirks without structured output.
    #[must_use]
    pub const fn without_response_format(mut self) -> Self {
        self.supports_response_format = false;
        self
    }
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
            framing: Framing::Sse,
        })
    }

    fn decoder(&self, _: &Target<'_>, _: &Request) -> Box<dyn Decoder> {
        Box::new(decode::ChatDecoder::new(self.quirks))
    }

    /// A Responses `call|item` id joins its legal halves with `_`, hashed
    /// past 40 characters; a host with a limit cuts any other id to it, and
    /// the rest keep theirs.
    fn tool_id(&self, id: &str) -> Option<String> {
        let fitted = if let Some((call, item)) = id.split_once('|') {
            let call = super::schema::legal_call_id(call, usize::MAX);
            let item = super::schema::legal_call_id(item, usize::MAX);
            let joined = if item.is_empty() {
                call.clone()
            } else {
                format!("{call}_{item}")
            };
            if joined.len() <= 40 {
                joined
            } else {
                let hash: String = super::schema::short_hash(id).chars().take(8).collect();
                let prefix: String = call.chars().take(40 - hash.len() - 1).collect();
                format!("{prefix}_{hash}")
            }
        } else {
            let limit = self.quirks.call_id_limit?;
            id.chars().take(limit).collect()
        };
        (fitted != id).then_some(fitted)
    }
}

#[cfg(test)]
mod tests;
