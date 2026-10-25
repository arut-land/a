//! The `OpenAI` Responses API (`POST /responses`), and the hosts that serve
//! it with their quirks.

mod decode;
pub(super) mod encode;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Error;
use crate::protocol::{Decoder, Encoded, Host, Hosted, Protocol, Target};
use crate::spec::{ProtocolKey, Reasoning, Request};
use crate::tag::Thought;

/// Which protocol this is.
pub const PROTOCOL: ProtocolKey = ProtocolKey::OpenaiResponses;

/// Where this protocol's options and part state go.
pub const KEY: &str = PROTOCOL.as_str();

/// What another model is shown of this protocol's reasoning: a reasoning
/// item's text is its summary, unless the item streamed raw reasoning
/// text (`reasoning_text`, from open-weight models) and no summary. Its
/// `encrypted_content` and id are shown to no one.
pub(crate) fn foreign(reasoning: &Reasoning) -> Option<Thought<'_>> {
    if reasoning.redacted {
        return None;
    }
    let item = crate::replay::item(&reasoning.meta, KEY);
    let has = |field: &str| {
        item.as_ref()
            .and_then(|item| item.get(field))
            .and_then(Value::as_array)
            .is_some_and(|parts| !parts.is_empty())
    };
    if has("content") && !has("summary") {
        Thought::raw(&reasoning.text)
    } else {
        Thought::summary(&reasoning.text)
    }
}

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

    const PROTOCOL: ProtocolKey = PROTOCOL;
    const HOSTS: &'static [(Host, Quirks)] = HOSTS;

    fn with(quirks: Quirks) -> Self {
        Self::new(quirks)
    }
}

impl Protocol for Responses {
    fn key(&self) -> ProtocolKey {
        PROTOCOL
    }

    fn encode(&self, target: &Target<'_>, request: &Request) -> Result<Encoded, Error> {
        let encoded = encode::body(self, target, request)?;
        Ok(Encoded {
            path: self.quirks.path.to_owned(),
            ..encoded
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
            compression: Some(transport::Compression::Zstd),
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
