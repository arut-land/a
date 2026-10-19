//! The `OpenAI` Responses API (`POST /responses`), and the hosts that serve
//! it with their quirks.

mod decode;
mod encode;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Error;
use crate::protocol::{Decoder, Encoded, Host, Hosted, Protocol, Target};
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

impl Hosted for Responses {
    type Quirks = Quirks;

    const KEY: &'static str = KEY;
    const HOSTS: &'static [(Host, Quirks)] = HOSTS;

    fn with(quirks: Quirks) -> Self {
        Self::new(quirks)
    }
}

impl Protocol for Responses {
    fn key(&self) -> &'static str {
        KEY
    }

    fn encode(&self, target: &Target<'_>, request: &Request) -> Result<Encoded, Error> {
        let (body, history) = encode::body(self, target, request)?;
        Ok(Encoded {
            path: self.quirks.path.to_owned(),
            body,
            history,
            headers: Vec::new(),
        })
    }

    fn decoder(&self, _: &Target<'_>, _: &Request) -> Box<dyn Decoder> {
        Box::new(decode::ResponsesDecoder::new())
    }

    /// The id cut to `[A-Za-z0-9_-]{1,64}`, without trailing `_`.
    fn tool_id(&self, id: &str) -> Option<String> {
        let legal = super::schema::legal_call_id(id, 64);
        super::schema::fit_call_id(id, legal.trim_end_matches('_'))
    }
}

/// The hosts that serve the Responses API, with their quirks.
pub const HOSTS: &[(Host, Quirks)] = &[
    (
        Host {
            base_url_env: &["OPENAI_BASE_URL"],
            ..Host::bearer("openai", "https://api.openai.com/v1", &["OPENAI_API_KEY"])
        },
        Quirks::OPENAI,
    ),
    (
        Host {
            base_url_env: &["OPENROUTER_BASE_URL"],
            ..Host::bearer(
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
            ..Host::bearer("xai", "https://api.x.ai", &["XAI_API_KEY"])
        },
        Quirks {
            path: "/v1/responses",
            instructions: Instructions::Input,
            ..Quirks::OPENAI
        },
    ),
];

/// The item types this protocol sends or tells apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemType {
    Message,
    Reasoning,
    FunctionCall,
    CustomToolCall,
    FunctionCallOutput,
    CustomToolCallOutput,
    ComputerCall,
    LocalShellCall,
    ShellCall,
    ApplyPatchCall,
    McpApprovalRequest,
    #[serde(other)]
    Other,
}

/// Items a client executes: kept, never sent back.
const CLIENT_EXECUTED: [ItemType; 5] = [
    ItemType::ComputerCall,
    ItemType::LocalShellCall,
    ItemType::ShellCall,
    ItemType::ApplyPatchCall,
    ItemType::McpApprovalRequest,
];

impl ItemType {
    /// The type an item's `type` field names; `None` when it names none.
    pub(crate) fn of(kind: Option<&Value>) -> Option<Self> {
        crate::json::tag(kind)
    }

    /// Whether an item of this type goes back to the model that produced it.
    pub(crate) fn replays(self) -> bool {
        !CLIENT_EXECUTED.contains(&self)
    }
}

/// The content and summary part types this protocol writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PartType {
    InputText,
    InputImage,
    InputFile,
    OutputText,
    SummaryText,
}

/// A response's or an item's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Completed,
    Incomplete,
    Failed,
    Cancelled,
    Queued,
    InProgress,
}

#[cfg(test)]
mod tests;
