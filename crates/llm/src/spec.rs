//! The provider-agnostic representation: what goes into a request, what a
//! stream says, and what a finished reply is. Every type here is plain serde
//! data, stable on disk, and knows nothing about any wire API.
//!
//! A message's kind decides what it may hold: only an assistant calls
//! tools, only a tool message answers them, and a system message is text.
//!
//! Provider state with no shared meaning (thinking signatures, encrypted
//! reasoning, item ids, Gemini thought signatures) rides on each part in
//! [`Meta`], keyed by the protocol that wrote it. It goes back only to the
//! protocol, provider and model that produced it (see [`Origin`]); every
//! other target rebuilds the part from its shared fields.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol-keyed provider state on a part or a reply.
pub type Meta = BTreeMap<String, Value>;

/// One model call.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub model: String,
    /// The conversation. Leading system messages are the system prompt;
    /// later ones are instructions given mid-conversation.
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "ToolChoice::is_auto")]
    pub tool_choice: ToolChoice,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Generation::is_default")]
    pub generation: Generation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    /// A key a provider may use to route requests sharing a prompt prefix to
    /// the same cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    /// Typed per-protocol options, keyed like [`Meta`]; each protocol reads
    /// only its own key.
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub options: Meta,
    /// Deep-merged over the encoded body last: the escape hatch for any
    /// field no option models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Generation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<NonZeroU32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<NonZeroU32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Effort>,
    /// A thinking budget in tokens, for protocols that take one instead of
    /// (or beside) an effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget: Option<NonZeroU32>,
}

impl Generation {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Format {
    Text,
    Json {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// JSON Schema of the input.
    pub schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "name", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Tool(String),
}

impl ToolChoice {
    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System(System),
    User(User),
    Assistant(Assistant),
    Tool(ToolMessage),
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self::System(System {
            content: vec![Text::new(text)],
        })
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::User(User {
            content: vec![UserPart::Text(Text::new(text))],
        })
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant(Assistant {
            content: vec![AssistantPart::Text(Text::new(text))],
            origin: None,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct System {
    pub content: Vec<Text>,
}

impl System {
    pub fn text(&self) -> String {
        join(self.content.iter().map(|text| text.text.as_str()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub content: Vec<UserPart>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserPart {
    Text(Text),
    Media(Media),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Assistant {
    /// The parts, in the order the provider produced them.
    pub content: Vec<AssistantPart>,
    /// Who produced it; `None` for a hand-built message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

impl Assistant {
    /// The concatenated answer text.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                AssistantPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The tool calls the caller must answer: every call the provider did
    /// not run itself.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|part| match part {
            AssistantPart::ToolCall(call) if !call.provider_executed => Some(call),
            _ => None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantPart {
    Text(Text),
    Reasoning(Reasoning),
    ToolCall(ToolCall),
    /// The result of a call the provider ran itself (a hosted tool).
    ToolResult(ToolResult),
    Media(Media),
    /// A provider item with no shared meaning (a hosted-tool step, a
    /// compaction record). Only its producer reads it, from `meta`.
    Opaque(Opaque),
}

impl AssistantPart {
    pub fn meta(&self) -> &Meta {
        match self {
            Self::Text(Text { meta, .. })
            | Self::Reasoning(Reasoning { meta, .. })
            | Self::ToolCall(ToolCall { meta, .. })
            | Self::ToolResult(ToolResult { meta, .. })
            | Self::Media(Media { meta, .. })
            | Self::Opaque(Opaque { meta }) => meta,
        }
    }

    pub fn meta_mut(&mut self) -> &mut Meta {
        match self {
            Self::Text(Text { meta, .. })
            | Self::Reasoning(Reasoning { meta, .. })
            | Self::ToolCall(ToolCall { meta, .. })
            | Self::ToolResult(ToolResult { meta, .. })
            | Self::Media(Media { meta, .. })
            | Self::Opaque(Opaque { meta }) => meta,
        }
    }
}

/// The answers to one assistant message's tool calls.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolMessage {
    pub results: Vec<ToolResult>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

impl Text {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            meta: Meta::new(),
        }
    }
}

/// Reasoning the model showed. Redacted reasoning has no readable text;
/// only its producer can replay it, from `meta`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Media {
    pub media_type: String,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    /// Base64 data.
    Data {
        data: String,
    },
    Url {
        url: String,
    },
    /// A file the provider holds, by its id.
    File {
        id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Map<String, Value>,
    /// The argument text, when it was no JSON object: `input` then holds
    /// what a cut-off object still states, and the caller answers with the
    /// parse error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
    /// The provider ran it; nothing local answers it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub provider_executed: bool,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub name: String,
    pub output: Output,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Output {
    Text(String),
    Json(Value),
    Content(Vec<UserPart>),
}

impl Default for Output {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl Output {
    /// The output as one string, for wires that take only text.
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Json(value) => value.to_string(),
            Self::Content(parts) => join(parts.iter().filter_map(|part| match part {
                UserPart::Text(text) => Some(text.text.as_str()),
                UserPart::Media(_) => None,
            })),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Opaque {
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

/// The wire protocol, provider and model that produced a message.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Origin {
    pub protocol: String,
    pub provider: String,
    pub model: String,
}

fn join<'a>(texts: impl Iterator<Item = &'a str>) -> String {
    texts.collect::<Vec<_>>().join("\n")
}

/// Token counts. `input` includes cached and cache-written tokens and
/// `output` includes reasoning, so `cache_read + cache_write <= input` and
/// `reasoning <= output`. A count the provider did not report is `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
}

impl Usage {
    /// `other`'s reported counts over this one's.
    pub fn merge(&mut self, other: Self) {
        self.input = other.input.or(self.input);
        self.output = other.output.or(self.output);
        self.cache_read = other.cache_read.or(self.cache_read);
        self.cache_write = other.cache_write.or(self.cache_write);
        self.reasoning = other.reasoning.or(self.reasoning);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Other,
}

/// How a reply ended: the normalized reason, the provider's own word, and
/// its explanation when it gave one (a refusal's, an incomplete reply's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finish {
    pub reason: Reason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Finish {
    pub fn new(reason: Reason, raw: impl Into<Option<String>>) -> Self {
        Self {
            reason,
            raw: raw.into(),
            message: None,
        }
    }
}

/// What kind of part an index streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Block {
    Text,
    Reasoning,
    ToolCall,
}

/// One step of a streamed reply. Parts are numbered by `index` as they
/// open; each opens, grows, and ends before the reply's `Finish`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// What the provider said about the reply, once it is known.
    Metadata {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// A part opens; a tool call opens once it names its tool.
    Start {
        index: u32,
        block: Block,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Text, reasoning text, or a fragment of a call's argument JSON.
    Delta { index: u32, text: String },
    /// A part ends. `text` replaces what streamed when the provider
    /// restated it; a call ends with its id and arguments.
    End {
        index: u32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<CallEnd>,
    },
    /// A whole part at once: media, a hosted tool's result, an opaque item.
    Part { index: u32, part: AssistantPart },
    /// Provider state for a part, merged key by key into what it has.
    Meta { index: u32, meta: Meta },
    Finish {
        finish: Finish,
        usage: Usage,
        #[serde(default, skip_serializing_if = "Meta::is_empty")]
        meta: Meta,
        /// The parts' order in the message, when it is not index order: a
        /// wire whose end states items the stream never announced.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        order: Option<Vec<u32>>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallEnd {
    pub id: String,
    pub input: serde_json::Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub provider_executed: bool,
}

/// A finished reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub message: Assistant,
    pub finish: Finish,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}
