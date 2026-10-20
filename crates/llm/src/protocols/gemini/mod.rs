//! Google's Gemini `GenerateContent` API, streamed
//! (`POST /v1beta/models/{model}:streamGenerateContent?alt=sse`), and the
//! hosts that serve it.
//!
//! Each reply part goes back to the model that produced it as Gemini sent
//! it, thought signature and all; Gemini 3 validates the signatures of the
//! function calls in the turn it is continuing.

mod decode;
mod encode;

use crate::error::Error;
use crate::protocol::{Decoder, Encoded, Host, Hosted, KeyStyle, Protocol, Target};
use crate::spec::{ProtocolKey, Request};

/// Which protocol this is.
pub const PROTOCOL: ProtocolKey = ProtocolKey::GoogleGemini;

/// Where this protocol's options and part state go.
pub const KEY: &str = PROTOCOL.as_str();

/// Which earlier thought signatures a request sends back.
///
/// Gemini 3 expands every signature a request carries back into the
/// reasoning it came from and bills it again as input: measured by rig on
/// gemini-3.8-flash, a 30-turn chat cost 42% less input without the
/// finished turns' signatures. Google asks for every signature back, and the
/// effect of leaving them out on answers is unmeasured, so [`Self::All`] is
/// the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThoughtReplay {
    #[default]
    All,
    /// Only the signatures at or after the newest user text: the turn whose
    /// function calls Gemini validates.
    CurrentTurn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quirks {
    /// Prefixes `/{model}:streamGenerateContent`.
    pub models_path: &'static str,
    pub thought_replay: ThoughtReplay,
}

impl Quirks {
    pub const GEMINI: Self = Self {
        models_path: "/v1beta/models",
        thought_replay: ThoughtReplay::All,
    };
}

impl Default for Quirks {
    fn default() -> Self {
        Self::GEMINI
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gemini {
    pub quirks: Quirks,
}

impl Gemini {
    pub const fn new(quirks: Quirks) -> Self {
        Self { quirks }
    }
}

impl Hosted for Gemini {
    type Quirks = Quirks;

    const PROTOCOL: ProtocolKey = PROTOCOL;
    const HOSTS: &'static [(Host, Quirks)] = HOSTS;

    fn with(quirks: Quirks) -> Self {
        Self::new(quirks)
    }
}

impl Protocol for Gemini {
    fn key(&self) -> ProtocolKey {
        PROTOCOL
    }

    fn encode(&self, target: &Target<'_>, request: &Request) -> Result<Encoded, Error> {
        let model = target.model.strip_prefix("models/").unwrap_or(target.model);
        let models = self.quirks.models_path;
        let encoded = encode::body(self, target, request)?;
        Ok(Encoded {
            // `alt=sse` makes the reply an event stream rather than one JSON
            // array of the same chunks.
            path: format!("{models}/{model}:streamGenerateContent?alt=sse"),
            ..encoded
        })
    }

    fn decoder(&self, _: &Target<'_>, _: &Request) -> Box<dyn Decoder> {
        Box::new(decode::GeminiDecoder::new())
    }

    /// Characters outside `[A-Za-z0-9_-]` become `_`, and at most 64 stay.
    /// Only models that take call ids are sent them.
    fn tool_id(&self, id: &str) -> Option<String> {
        let legal = super::schema::legal_call_id(id, 64);
        (legal != id).then_some(legal)
    }
}

/// The hosts that serve `GenerateContent`, with their quirks.
pub const HOSTS: &[(Host, Quirks)] = &[(
    Host {
        id: "gemini",
        base_url: "https://generativelanguage.googleapis.com",
        key_env: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        base_url_env: &["GEMINI_BASE_URL"],
        key: KeyStyle::Header("x-goog-api-key"),
        keyless: false,
    },
    Quirks::GEMINI,
)];

#[cfg(test)]
mod tests;
