//! The provider-agnostic representation: what goes into a request, what a
//! stream says, and what a finished reply is. Every type here is plain serde
//! data, stable on disk, and knows nothing about any wire API. A logged
//! reply is read by [`Response::from_json`], the one codec of the shipped
//! binary; the derived `Deserialize` of the larger types exists for tests
//! (fixtures, and that the reader agrees with the derive).
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
use std::fmt;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};

use crate::conversation::Shared;

pub use ::json::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use smol_str::SmolStr;

pub use self::meta::Meta;
pub use crate::conversation::Messages;

/// A wire protocol by name: what an [`Origin`] names, and the key a
/// request's options and a part's state go under. Four exist; a fifth
/// cannot be spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ProtocolKey {
    #[serde(rename = "openai.chat")]
    OpenaiChat,
    #[serde(rename = "openai.responses")]
    OpenaiResponses,
    #[serde(rename = "anthropic.messages")]
    AnthropicMessages,
    #[serde(rename = "google.gemini")]
    GoogleGemini,
}

impl ProtocolKey {
    /// The key as it is written: in a log's `origin` and `meta`, in a
    /// request's options.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai.chat",
            Self::OpenaiResponses => "openai.responses",
            Self::AnthropicMessages => "anthropic.messages",
            Self::GoogleGemini => "google.gemini",
        }
    }
}

impl fmt::Display for ProtocolKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-protocol options, each protocol's as body fields, keyed by protocol.
pub type Options = BTreeMap<ProtocolKey, Value>;

/// One model call.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct Request {
    pub model: SmolStr,
    /// The conversation. Leading system messages are the system prompt;
    /// later ones are instructions given mid-conversation.
    pub messages: Messages,
    #[serde(default, skip_serializing_if = "Tools::is_empty")]
    pub tools: Tools,
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
    #[serde(default, skip_serializing_if = "Options::is_empty")]
    pub options: Options,
    /// Deep-merged over the encoded body last: the escape hatch for any
    /// field no option models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
}

impl Request {
    /// The options under protocol `key`, as body fields.
    pub fn options_for(&self, key: ProtocolKey) -> serde_json::Map<String, Value> {
        self.options
            .get(&key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
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

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
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

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct Tool {
    pub name: SmolStr,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// JSON Schema of the input.
    pub schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// The tools a request offers: shared, as every request of a tool set
/// offers the same, with their definitions as each wire encodes them, made
/// for the first request that offers them and kept for every later one.
#[derive(Clone, Default)]
pub struct Tools(Arc<Set>);

#[derive(Default)]
struct Set {
    tools: Vec<Tool>,
    /// Each tool's definition, as bytes, per wire it was encoded for.
    encoded: Mutex<Vec<(ToolWire, Arc<[Shared]>)>>,
}

/// A wire's way of writing a tool definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolWire {
    Chat { strict: bool },
    Responses { strict: bool },
    Messages { strict: bool, eager: bool },
    Gemini,
}

impl Tools {
    fn is_empty(&self) -> bool {
        self.0.tools.is_empty()
    }

    /// The tools' definitions as `wire` writes them, one byte run each:
    /// `encode` runs once per tool, for the first request.
    pub(crate) fn encoded(&self, wire: ToolWire, encode: impl Fn(&Tool) -> Value) -> Arc<[Shared]> {
        let mut kept = self
            .0
            .encoded
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((_, run)) = kept.iter().find(|(w, _)| *w == wire) {
            return Arc::clone(run);
        }
        let run: Arc<[Shared]> = (self.0.tools.iter())
            .map(|tool| serde_json::to_vec(&encode(tool)).unwrap_or_default().into())
            .collect();
        kept.push((wire, Arc::clone(&run)));
        run
    }
}

impl std::ops::Deref for Tools {
    type Target = [Tool];

    fn deref(&self) -> &[Tool] {
        &self.0.tools
    }
}

/// Two tool sets are equal when their tools are: what each wire made of
/// them follows.
impl PartialEq for Tools {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl fmt::Debug for Tools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.tools.fmt(f)
    }
}

impl From<Vec<Tool>> for Tools {
    fn from(tools: Vec<Tool>) -> Self {
        Self(Arc::new(Set {
            tools,
            encoded: Mutex::default(),
        }))
    }
}

impl FromIterator<Tool> for Tools {
    fn from_iter<I: IntoIterator<Item = Tool>>(tools: I) -> Self {
        Vec::from_iter(tools).into()
    }
}

impl Serialize for Tools {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.0.tools.iter())
    }
}

#[cfg(test)]
impl<'de> Deserialize<'de> for Tools {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Vec::<Tool>::deserialize(d).map(Into::into)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "name", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Tool(SmolStr),
}

impl ToolChoice {
    fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
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

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct System {
    pub content: Vec<Text>,
}

impl System {
    /// The text, its parts joined by newlines.
    pub fn text(&self) -> String {
        let mut texts = self.content.iter().map(|text| text.text.as_str());
        let mut joined = texts.next().unwrap_or_default().to_owned();
        for text in texts {
            joined.push('\n');
            joined.push_str(text);
        }
        joined
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct User {
    pub content: Vec<UserPart>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserPart {
    Text(Text),
    Media(Media),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
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

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct ToolMessage {
    pub results: Vec<ToolResult>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
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
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct Reasoning {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct Media {
    pub media_type: SmolStr,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
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
        id: SmolStr,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct ToolCall {
    pub id: SmolStr,
    pub name: SmolStr,
    /// The arguments, a JSON object as compact text with its keys in
    /// order: what the model sent, read once, and passed on as it is to
    /// the log, the tool and every later request.
    #[serde(deserialize_with = "object")]
    pub input: Json,
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

/// A call's arguments as a derived `Deserialize` reads them, buffered: a
/// JSON object, through a tree. [`Response::from_json`] keeps the text.
#[cfg(test)]
fn object<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
    let object = serde_json::Map::<String, Value>::deserialize(d)?;
    Json::encode(&object).map_err(serde::de::Error::custom)
}

impl ToolCall {
    /// The `input` its arguments hold, as its text: a custom tool's
    /// free-form input. Only that field is read.
    pub(crate) fn input_field(&self) -> Option<&serde_json::value::RawValue> {
        #[derive(Deserialize)]
        struct Custom<'a> {
            #[serde(borrow)]
            input: Option<&'a serde_json::value::RawValue>,
        }
        self.input.parse::<Custom<'_>>().ok()?.input
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct ToolResult {
    pub id: SmolStr,
    pub name: SmolStr,
    pub output: Output,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Output {
    Text(String),
    /// A tool's output as it was stored, sent as its text.
    Json(#[serde(deserialize_with = "Json::buffered")] Json),
    Content(Vec<UserPart>),
}

impl Default for Output {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct Opaque {
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

/// The wire protocol, provider and model that produced a message.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Origin {
    pub protocol: ProtocolKey,
    pub provider: SmolStr,
    pub model: SmolStr,
}

/// Token counts, as the provider reported them; a count it left out is
/// `None`. Totals include their parts: `input` holds every input token
/// (uncached, read from a cache, written to one, added by hosted tools) and
/// `output` every output token (reasoning and audio included). The
/// provider's own usage object rides on the reply's `meta` under its
/// protocol's key, as `usage`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    /// The part of `input` read from the provider's cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    /// The part of `input` written to the provider's cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    /// The part of `cache_write` kept for the long retention (Anthropic's
    /// one hour), which bills higher.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_long: Option<u64>,
    /// The part of `input` the provider's hosted tools added (Gemini's
    /// tool-use prompt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<u64>,
    /// The part of `input` that was audio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_input: Option<u64>,
    /// The part of `output` spent reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    /// The part of `output` that was audio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_output: Option<u64>,
    /// Requests the provider's hosted tools made (web searches, fetches,
    /// code runs), which bill per request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hosted_tool_calls: Option<u64>,
}

/// The counts a [`Usage`] holds, each naming its field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    Input,
    Output,
    CacheRead,
    CacheWrite,
    CacheWriteLong,
    ToolInput,
    AudioInput,
    Reasoning,
    AudioOutput,
    HostedToolCalls,
}

impl Count {
    pub const ALL: [Self; 10] = [
        Self::Input,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite,
        Self::CacheWriteLong,
        Self::ToolInput,
        Self::AudioInput,
        Self::Reasoning,
        Self::AudioOutput,
        Self::HostedToolCalls,
    ];

    /// The count field `key` names.
    pub(crate) fn named(key: &str) -> Option<Self> {
        Some(match key {
            "input" => Self::Input,
            "output" => Self::Output,
            "cache_read" => Self::CacheRead,
            "cache_write" => Self::CacheWrite,
            "cache_write_long" => Self::CacheWriteLong,
            "tool_input" => Self::ToolInput,
            "audio_input" => Self::AudioInput,
            "reasoning" => Self::Reasoning,
            "audio_output" => Self::AudioOutput,
            "hosted_tool_calls" => Self::HostedToolCalls,
            _ => return None,
        })
    }
}

impl Usage {
    #[inline(never)]
    pub fn encode_counts(counts: &[Option<u64>; 10]) -> serde_json::Result<Json> {
        Json::encode(counts)
    }

    #[inline(never)]
    pub fn decode_counts(json: &Json) -> serde_json::Result<[Option<u64>; 10]> {
        json.parse()
    }

    #[inline(never)]
    pub fn counts(mut self) -> [Option<u64>; 10] {
        Count::ALL.map(|count| *self.count(count))
    }

    /// The field `count` names.
    pub fn count(&mut self, count: Count) -> &mut Option<u64> {
        match count {
            Count::Input => &mut self.input,
            Count::Output => &mut self.output,
            Count::CacheRead => &mut self.cache_read,
            Count::CacheWrite => &mut self.cache_write,
            Count::CacheWriteLong => &mut self.cache_write_long,
            Count::ToolInput => &mut self.tool_input,
            Count::AudioInput => &mut self.audio_input,
            Count::Reasoning => &mut self.reasoning,
            Count::AudioOutput => &mut self.audio_output,
            Count::HostedToolCalls => &mut self.hosted_tool_calls,
        }
    }

    /// The counts `table` names read from `usage`, a provider's usage
    /// object, by JSON pointer; one absent, or not a count, stays `None`.
    pub(crate) fn read(usage: &Value, table: &[(Count, &str)]) -> Self {
        use crate::json::Lenient;
        let mut read = Self::default();
        for (count, pointer) in table {
            *read.count(*count) = usage.at(pointer).and_then(Value::as_u64_lenient);
        }
        read
    }

    /// The part of `input` neither read from nor written to a cache.
    pub fn uncached_input(&self) -> Option<u64> {
        let cached = self.cache_read.unwrap_or(0) + self.cache_write.unwrap_or(0);
        self.input.map(|input| input.saturating_sub(cached))
    }

    /// The part of `output` that was the answer: neither reasoning nor audio.
    pub fn answer_output(&self) -> Option<u64> {
        let other = self.reasoning.unwrap_or(0) + self.audio_output.unwrap_or(0);
        self.output.map(|output| output.saturating_sub(other))
    }

    pub fn total(&self) -> Option<u64> {
        Some(self.input? + self.output?)
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

impl Reason {
    /// The reason `table`, a protocol's vocabulary, gives `word`.
    pub(crate) fn listed(table: &[(&str, Self)], word: &str) -> Option<Self> {
        let found = table.iter().find(|(name, _)| *name == word);
        found.map(|(_, reason)| *reason)
    }

    /// [`Self::listed`], `Other` for a word the table does not know.
    pub(crate) fn of(table: &[(&str, Self)], word: &str) -> Self {
        Self::listed(table, word).unwrap_or(Self::Other)
    }
}

/// How a reply ended: the normalized reason, the provider's own word, and
/// its explanation when it gave one (a refusal's, an incomplete reply's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finish {
    pub reason: Reason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<SmolStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Finish {
    pub fn new(reason: Reason, raw: impl Into<Option<SmolStr>>) -> Self {
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
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// What the provider said about the reply, once it is known.
    Metadata {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<SmolStr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<SmolStr>,
    },
    /// A part opens; a tool call opens once it names its tool.
    Start {
        index: u32,
        block: Block,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<SmolStr>,
    },
    /// Text, reasoning text, or a fragment of a call's argument JSON.
    Delta { index: u32, text: String },
    /// A part ends, as what it was.
    End { index: u32, ended: Ended },
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

/// What a part ended as. Text and reasoning end with their whole `text`
/// (what streamed, or what the provider restated), moved rather than
/// copied: deltas carry pieces, and only the end the whole. A call ends
/// with its id and arguments.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum Ended {
    Text { text: String },
    Reasoning { text: String, redacted: bool },
    Call(CallEnd),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct CallEnd {
    pub id: SmolStr,
    #[serde(deserialize_with = "object")]
    pub input: Json,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub provider_executed: bool,
}

/// A finished reply.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
pub struct Response {
    pub message: Assistant,
    pub finish: Finish,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<SmolStr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<SmolStr>,
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

mod meta;
mod read;

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// A tool set never changes, so each wire encodes its definitions
    /// once, as bytes every copy of the set shares; another wire, or the
    /// same one strict, encodes its own.
    #[test]
    fn a_tool_set_is_encoded_once_per_wire() {
        let tool = |name: &str| Tool {
            name: name.into(),
            description: String::new(),
            schema: serde_json::json!({ "type": "object" }),
            strict: None,
        };
        let tools = Tools::from(vec![tool("a"), tool("b")]);
        let copy = tools.clone();
        let made = Cell::new(0);
        let encode = |tool: &Tool| {
            made.set(made.get() + 1);
            serde_json::json!({ "name": tool.name })
        };
        let chat = ToolWire::Chat { strict: false };
        for tools in [&tools, &copy, &tools] {
            let run = tools.encoded(chat, encode);
            let texts: Vec<&str> = run
                .iter()
                .map(|bytes| std::str::from_utf8(bytes).unwrap())
                .collect();
            assert_eq!(texts, [r#"{"name":"a"}"#, r#"{"name":"b"}"#]);
        }
        assert_eq!(made.get(), 2);
        tools.encoded(ToolWire::Chat { strict: true }, encode);
        assert_eq!(made.get(), 4);
        assert_eq!(tools, copy, "the tools, not what was made of them");
    }
}
