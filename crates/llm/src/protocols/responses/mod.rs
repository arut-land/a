//! The `OpenAI` Responses API (`POST /responses`), and the hosts that serve
//! it with their quirks.

mod decode;
mod encode;

use crate::error::Error;
use crate::protocol::{Decoder, Encoded, Framing, Host, KeyStyle, Protocol, Target};
use crate::spec::Request;

/// Where this protocol's options and part state go.
pub const KEY: &str = "openai.responses";

/// Where the request's system instructions go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Instructions {
    /// The leading run of system messages in the top-level `instructions`;
    /// later ones stay in `input`.
    #[default]
    Leading,
    /// Every system message in `instructions`, for a backend that rejects
    /// the `system` role in `input`.
    All,
    /// Every system message in `input`, for a backend with no
    /// `instructions`.
    Input,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quirks {
    pub path: &'static str,
    pub instructions: Instructions,
    /// Whether tools that do not say are sent strict.
    pub strict_tools: bool,
}

impl Quirks {
    pub const OPENAI: Self = Self {
        path: "/responses",
        instructions: Instructions::Leading,
        strict_tools: false,
    };
}

impl Default for Quirks {
    fn default() -> Self {
        Self::OPENAI
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Responses {
    pub quirks: Quirks,
}

impl Responses {
    pub const fn new(quirks: Quirks) -> Self {
        Self { quirks }
    }
}

impl Protocol for Responses {
    fn key(&self) -> &'static str {
        KEY
    }

    fn encode(&self, target: &Target<'_>, request: &Request) -> Result<Encoded, Error> {
        Ok(Encoded {
            path: self.quirks.path.to_owned(),
            body: encode::body(self, target, request)?,
            headers: Vec::new(),
            framing: Framing::Sse,
        })
    }

    fn decoder(&self, _: &Target<'_>, _: &Request) -> Box<dyn Decoder> {
        Box::new(decode::ResponsesDecoder::new())
    }

    fn tool_id(&self, id: &str) -> Option<String> {
        let legal = super::schema::legal_call_id(id, 64);
        let fitted = match legal.trim_end_matches('_') {
            // An id with nothing legal in it still names its call.
            "" => super::schema::short_hash(id),
            trimmed => trimmed.to_owned(),
        };
        (fitted != id).then_some(fitted)
    }
}

const fn host(id: &'static str, base_url: &'static str, key_env: &'static [&'static str]) -> Host {
    Host {
        id,
        base_url,
        key_env,
        base_url_env: &[],
        key: KeyStyle::Bearer,
        keyless: false,
    }
}

/// The hosts that serve the Responses API, with their quirks.
pub const HOSTS: &[(Host, Quirks)] = &[
    (
        Host {
            base_url_env: &["OPENAI_BASE_URL"],
            ..host("openai", "https://api.openai.com/v1", &["OPENAI_API_KEY"])
        },
        Quirks::OPENAI,
    ),
    (
        Host {
            base_url_env: &["OPENROUTER_BASE_URL"],
            ..host(
                "openrouter",
                "https://openrouter.ai/api/v1",
                &["OPENROUTER_API_KEY"],
            )
        },
        Quirks {
            instructions: Instructions::Input,
            ..Quirks::OPENAI
        },
    ),
    (
        Host {
            base_url_env: &["XAI_BASE_URL"],
            ..host("xai", "https://api.x.ai", &["XAI_API_KEY"])
        },
        Quirks {
            path: "/v1/responses",
            instructions: Instructions::Input,
            ..Quirks::OPENAI
        },
    ),
];

#[cfg(test)]
mod tests;
