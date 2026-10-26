//! The Anthropic Messages API (`POST /v1/messages`), and the hosts that serve
//! it with their quirks: Anthropic itself and the Anthropic-format endpoints
//! of Z.AI, `MiniMax`, Moonshot and Xiaomi `MiMo`.
//!
//! Typed options under [`KEY`] go into the body as the caller writes them,
//! merged over what the request maps to, except these the encoder reads:
//!
//! - `tools`: provider tools (server tools, MCP toolsets), sent after the
//!   request's own tools as given.
//! - `cache_control`: the top-level cache marker, an `ephemeral` marker
//!   with no `ttl`, `"5m"` or `"1h"`; it takes one breakpoint of four.
//! - `prompt_caching: true`: breakpoints on the last tool, the system prompt
//!   and the last block of the last non-assistant message.
//! - `static_prefix_cache_ttl`: `"5m"` or `"1h"`, a breakpoint on the last
//!   tool and the system prompt at that lifetime, apart from the tail's.
//! - `betas`: `anthropic-beta` flags, sent beside the ones a request needs.

mod cache;
mod decode;
mod encode;
mod options;
mod strict;

use serde_json::Value;

use crate::error::{Error, Kind};
use crate::protocol::{Decoder, Encoded, Host, Hosted, KeyStyle, Protocol, Target};
use crate::spec::{Origin, ProtocolKey, Reasoning, Request};
use crate::tag::Thought;

pub use crate::models::anthropic::{default_max_tokens, rejects_forced_tool_choice};

/// Which protocol this is.
pub const PROTOCOL: ProtocolKey = ProtocolKey::AnthropicMessages;

/// Where this protocol's options and part state go.
pub const KEY: &str = PROTOCOL.as_str();

/// What another model is shown of this protocol's reasoning: Claude 4 and
/// later stream a summary of their thinking, while Claude 3.7 and the
/// other models served over this wire stream it raw. Redacted thinking is
/// ciphertext and is shown to no one.
pub(crate) fn foreign<'a>(reasoning: &'a Reasoning, origin: &Origin) -> Option<Thought<'a>> {
    if reasoning.redacted {
        return None;
    }
    if crate::models::anthropic::summarizes_thinking(&origin.model) {
        Thought::summary(&reasoning.text)
    } else {
        Thought::raw(&reasoning.text)
    }
}

/// The `anthropic-version` every request names.
pub const VERSION: &str = "2023-06-01";

/// The beta that lets a request ask Anthropic to drop a thinking block bound
/// to another context instead of rejecting the request.
pub const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// The beta that streams tool input as it is written, for a host that
/// rejects the per-tool `eager_input_streaming` field.
pub const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";

/// Whose Messages API this is: the generation options each takes differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Anthropic,
    ZAi,
    MiniMax,
    Moonshot,
    MiMo,
    /// A gateway no option mapping is known for: only the fields every
    /// dialect takes are sent.
    Other,
}

/// How a request with tools asks for each call's input as the model writes
/// it. Without it, Anthropic holds a long argument back and sends it in one
/// burst.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolInputStreaming {
    /// `eager_input_streaming: true` on each of the request's tools.
    #[default]
    Eager,
    /// The [`FINE_GRAINED_TOOL_STREAMING_BETA`] flag, for a host that
    /// rejects the per-tool field.
    BetaHeader,
    /// Neither: the host streams tool input as it chooses.
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quirks {
    pub dialect: Dialect,
    /// The `max_tokens` a request that sets none takes (Anthropic requires
    /// the field): one ceiling for every model, all a gateway documents, or
    /// `None` for Anthropic's published per-model output limits.
    pub max_tokens: Option<u32>,
    /// Whether the host implements Anthropic's constrained tool schemas; a
    /// strict tool goes unchanged to one that does not.
    pub strict_tool_schemas: bool,
    /// Whether the host takes thinking back without a signature, as Kimi
    /// does. Elsewhere unsigned thinking replays as text.
    pub unsigned_thinking: bool,
    pub tool_input_streaming: ToolInputStreaming,
}

impl Quirks {
    pub const ANTHROPIC: Self = Self {
        dialect: Dialect::Anthropic,
        max_tokens: None,
        strict_tool_schemas: true,
        unsigned_thinking: false,
        tool_input_streaming: ToolInputStreaming::Eager,
    };

    /// A gateway's: 4096 output tokens by default, no constrained tool
    /// schemas, eager tool input.
    pub const GATEWAY: Self = Self {
        dialect: Dialect::Other,
        max_tokens: Some(4096),
        strict_tool_schemas: false,
        ..Self::ANTHROPIC
    };

    pub fn default_max_tokens(&self, model: &str) -> Option<u32> {
        self.max_tokens.or_else(|| default_max_tokens(model))
    }
}

impl Default for Quirks {
    fn default() -> Self {
        Self::ANTHROPIC
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Messages {
    pub quirks: Quirks,
}

impl Messages {
    pub const fn new(quirks: Quirks) -> Self {
        Self { quirks }
    }
}

/// The path to post to under `base_url`, which may already name the API
/// version or the endpoint, as pasted from the docs.
fn path(base_url: &str) -> &'static str {
    if base_url.ends_with("/v1/messages") {
        ""
    } else if base_url.ends_with("/v1") {
        "/messages"
    } else {
        "/v1/messages"
    }
}

impl Hosted for Messages {
    type Quirks = Quirks;

    const PROTOCOL: ProtocolKey = PROTOCOL;
    const HOSTS: &'static [(Host, Quirks)] = HOSTS;

    fn with(quirks: Quirks) -> Self {
        Self::new(quirks)
    }
}

impl Protocol for Messages {
    fn key(&self) -> ProtocolKey {
        PROTOCOL
    }

    fn encode(&self, target: &Target<'_>, request: &Request) -> Result<Encoded, Error> {
        let (encoded, betas) = encode::body(self.quirks, target, request)?;
        let mut headers = vec![("anthropic-version", VERSION.to_owned())];
        headers.extend(betas.map(|betas| ("anthropic-beta", betas)));
        Ok(Encoded {
            path: path(&target.provider.base_url).to_owned(),
            headers,
            ..encoded
        })
    }

    fn decoder(&self, _: &Target<'_>, _: &Request) -> Box<dyn Decoder> {
        Box::new(decode::MessagesDecoder::new(self.quirks.unsigned_thinking))
    }

    /// Anthropic takes call ids of `[A-Za-z0-9_-]`, at most 64 long.
    fn tool_id(&self, id: &str) -> Option<String> {
        super::schema::fit_call_id(id, &super::schema::legal_call_id(id, 64))
    }

    /// The status decides, refined by the envelope's `error.type` where it
    /// says more (a `billing_error`, an `overloaded_error` on any status).
    fn error(&self, status: u16, headers: &http::HeaderMap, body: &[u8]) -> Error {
        let mut error = Error::from_status(status, headers, body);
        let envelope: Option<Value> = serde_json::from_slice(body).ok();
        let stated = envelope
            .as_ref()
            .and_then(|envelope| envelope.pointer("/error/type"));
        let stated = stated.and_then(Value::as_str);
        match stated.map(|kind| Error::provider(Some(kind), String::new()).kind) {
            None | Some(Kind::Other | Kind::InvalidRequest | Kind::ContextOverflow) => {}
            // A gateway may wrap a rejected request as an `api_error`: a 4xx
            // stays the client's fault.
            Some(Kind::Server) if (400..500).contains(&status) => {}
            Some(kind) => error.kind = kind,
        }
        error
    }
}

/// A gateway's host and quirks: its key in `<ENV>_API_KEY`, its base URL
/// in `<ENV>_ANTHROPIC_API_BASE`.
macro_rules! gateway {
    ($id:literal, $base_url:literal, $env:literal, $($quirk:ident: $value:expr),+ $(,)?) => {
        (
            Host {
                id: $id,
                base_url: $base_url,
                key_env: &[concat!($env, "_API_KEY")],
                base_url_env: &[concat!($env, "_ANTHROPIC_API_BASE")],
                compression: None,
                ..ANTHROPIC.0
            },
            Quirks { $($quirk: $value,)+ ..Quirks::GATEWAY },
        )
    };
}

const ANTHROPIC: (Host, Quirks) = (
    Host {
        id: "anthropic",
        base_url: "https://api.anthropic.com",
        key_env: &["ANTHROPIC_API_KEY"],
        base_url_env: &["ANTHROPIC_BASE_URL"],
        key: KeyStyle::Header("x-api-key"),
        keyless: false,
        compression: Some(transport::Compression::Gzip),
    },
    Quirks::ANTHROPIC,
);

/// The hosts that serve the Messages API, with their quirks.
pub const HOSTS: &[(Host, Quirks)] = &[
    ANTHROPIC,
    gateway!("zai", "https://api.z.ai/api/anthropic", "ZAI", dialect: Dialect::ZAi),
    gateway!("minimax", "https://api.minimax.io/anthropic", "MINIMAX", dialect: Dialect::MiniMax),
    gateway!(
        "moonshot",
        "https://api.moonshot.ai/anthropic",
        "MOONSHOT",
        dialect: Dialect::Moonshot,
        unsigned_thinking: true,
    ),
    gateway!("xiaomimimo", "https://api.xiaomimimo.com/anthropic", "XIAOMI_MIMO", dialect: Dialect::MiMo),
];

#[cfg(test)]
mod tests;
