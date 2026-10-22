//! Shared field readers avoid buffering tagged parts and preserve opaque
//! provider state exactly as the log recorded it. All owning codecs use
//! byte readers so response and conversation decoding share instantiations.

use std::fmt;

use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Error;
use serde_json::value::RawValue;

use super::meta::Raw;
use super::{
    Assistant, AssistantPart, Count, Finish, Json, Media, Meta, Opaque, Origin, Output,
    ProtocolKey, Reason, Reasoning, Response, SmolStr, Source, Text, ToolCall, ToolResult, Usage,
    UserPart,
};
use crate::json::{Fields, fields, next};

type Result<T> = std::result::Result<T, Error>;

impl Response {
    /// A reply as this crate writes it, read from its text.
    pub fn from_json(text: &str) -> serde_json::Result<Self> {
        let mut json = serde_json::Deserializer::from_slice(text.as_bytes());
        let reply: Reply = fields(&mut json)?;
        json.end()?;
        Ok(Self {
            message: required(reply.message, "message")?,
            finish: required(reply.finish, "finish")?,
            usage: required(reply.usage, "usage")?,
            id: reply.id,
            model: reply.model,
            meta: reply.meta.0,
        })
    }
}

impl crate::Messages {
    pub fn from_json(text: &str) -> serde_json::Result<Self> {
        let messages: Vec<Message> = serde_json::from_slice(text.as_bytes())?;
        let mut built = Self::default();
        for mut message in messages {
            let role = required(message.role.take(), "role")?;
            built.push(message.build(&role)?);
        }
        Ok(built)
    }
}

/// A call's arguments, which are an object.
fn object(input: Json) -> Result<Json> {
    if input.raw().starts_with('{') {
        Ok(input)
    } else {
        Err(Error::custom("a call's input is a JSON object"))
    }
}

fn required<T>(field: Option<T>, key: &'static str) -> Result<T> {
    field.ok_or_else(|| Error::missing_field(key))
}

/// A tag no variant of `kinds` names.
fn unknown(tag: &str, kinds: &'static [&'static str]) -> Error {
    Error::unknown_variant(tag, kinds)
}

/// Reads an object as `T`'s [`Fields`]: the types here are read only that
/// way.
macro_rules! read_as_fields {
    ($($type:ident),*) => {$(
        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(json: D) -> std::result::Result<Self, D::Error> {
                fields(json)
            }
        }
    )*};
}

read_as_fields!(
    Message,
    OriginFields,
    FinishFields,
    UsageFields,
    Part,
    OutputFields,
    SourceFields
);

#[derive(Default)]
struct Reply {
    message: Option<Assistant>,
    finish: Option<Finish>,
    usage: Option<Usage>,
    id: Option<SmolStr>,
    model: Option<SmolStr>,
    meta: State,
}

impl<'de> Fields<'de> for Reply {
    fn field<A: MapAccess<'de>>(
        &mut self,
        key: &str,
        map: &mut A,
    ) -> std::result::Result<bool, A::Error> {
        match key {
            "message" => {
                let message: Message = next(map)?;
                let super::Message::Assistant(assistant) =
                    message.build("assistant").map_err(A::Error::custom)?
                else {
                    unreachable!()
                };
                self.message = Some(assistant);
            }
            "finish" => {
                let finish: FinishFields = next(map)?;
                self.finish = Some(finish.build().map_err(A::Error::custom)?);
            }
            "usage" => self.usage = Some(next::<_, UsageFields>(map)?.0),
            "id" => self.id = next(map)?,
            "model" => self.model = next(map)?,
            "meta" => self.meta = next(map)?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

#[derive(Default)]
struct Message {
    role: Option<SmolStr>,
    content: Option<Vec<Part>>,
    results: Vec<Part>,
    origin: Option<OptionalOrigin>,
}

impl<'de> Fields<'de> for Message {
    fn field<A: MapAccess<'de>>(
        &mut self,
        key: &str,
        map: &mut A,
    ) -> std::result::Result<bool, A::Error> {
        match key {
            "role" => self.role = Some(next(map)?),
            "results" => self.results = next(map)?,
            "content" => self.content = Some(next(map)?),
            "origin" => self.origin = Some(next(map)?),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl Message {
    fn build(self, role: &str) -> Result<super::Message> {
        let mut message = match role {
            "system" => super::Message::System(super::System {
                content: Vec::new(),
            }),
            "user" => super::Message::User(super::User {
                content: Vec::new(),
            }),
            "assistant" => super::Message::Assistant(Assistant {
                content: Vec::new(),
                origin: self.origin.and_then(|o| o.0),
            }),
            "tool" => super::Message::Tool(super::ToolMessage {
                results: Vec::new(),
            }),
            role => return Err(unknown(role, &["system", "user", "assistant", "tool"])),
        };
        let parts = if role == "tool" {
            self.results
        } else {
            required(self.content, "content")?
        };
        for part in parts {
            match &mut message {
                super::Message::System(system) => system.content.push(part.text()?),
                super::Message::User(user) => user.content.push(part.user()?),
                super::Message::Assistant(assistant) => assistant.content.push(part.build()?),
                super::Message::Tool(tool) => tool.results.push(part.tool_result()?),
            }
        }
        Ok(message)
    }
}

/// An origin, `None` for `null`.
struct OptionalOrigin(Option<Origin>);

impl<'de> Deserialize<'de> for OptionalOrigin {
    fn deserialize<D: Deserializer<'de>>(json: D) -> std::result::Result<Self, D::Error> {
        let origin: Option<OriginFields> = Deserialize::deserialize(json)?;
        let origin = origin.map(OriginFields::build).transpose();
        origin.map(OptionalOrigin).map_err(D::Error::custom)
    }
}

#[derive(Default)]
struct OriginFields {
    protocol: Option<ProtocolKey>,
    provider: Option<SmolStr>,
    model: Option<SmolStr>,
}

impl<'de> Fields<'de> for OriginFields {
    fn field<A: MapAccess<'de>>(
        &mut self,
        key: &str,
        map: &mut A,
    ) -> std::result::Result<bool, A::Error> {
        match key {
            "protocol" => self.protocol = Some(next(map)?),
            "provider" => self.provider = Some(next(map)?),
            "model" => self.model = Some(next(map)?),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl OriginFields {
    fn build(self) -> Result<Origin> {
        Ok(Origin {
            protocol: required(self.protocol, "protocol")?,
            provider: required(self.provider, "provider")?,
            model: required(self.model, "model")?,
        })
    }
}

#[derive(Default)]
struct FinishFields {
    reason: Option<SmolStr>,
    raw: Option<SmolStr>,
    message: Option<String>,
}

impl<'de> Fields<'de> for FinishFields {
    fn field<A: MapAccess<'de>>(
        &mut self,
        key: &str,
        map: &mut A,
    ) -> std::result::Result<bool, A::Error> {
        match key {
            "reason" => self.reason = Some(next(map)?),
            "raw" => self.raw = next(map)?,
            "message" => self.message = next(map)?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl FinishFields {
    fn build(self) -> Result<Finish> {
        const REASONS: &[&str] = &[
            "stop",
            "length",
            "tool_calls",
            "content_filter",
            "error",
            "other",
        ];
        let reason = match required(self.reason, "reason")?.as_str() {
            "stop" => Reason::Stop,
            "length" => Reason::Length,
            "tool_calls" => Reason::ToolCalls,
            "content_filter" => Reason::ContentFilter,
            "error" => Reason::Error,
            "other" => Reason::Other,
            reason => return Err(unknown(reason, REASONS)),
        };
        Ok(Finish {
            reason,
            raw: self.raw,
            message: self.message,
        })
    }
}

#[derive(Default)]
struct UsageFields(Usage);

impl<'de> Fields<'de> for UsageFields {
    fn field<A: MapAccess<'de>>(
        &mut self,
        key: &str,
        map: &mut A,
    ) -> std::result::Result<bool, A::Error> {
        let Some(count) = Count::named(key) else {
            return Ok(false);
        };
        *self.0.count(count) = next(map)?;
        Ok(true)
    }
}

/// Provider state, each key's value kept as its text.
#[derive(Default)]
struct State(Meta);

impl<'de> Deserialize<'de> for State {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Read;
        impl<'de> Visitor<'de> for Read {
            type Value = State;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a map")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<State, A::Error> {
                let mut meta = Meta::new();
                while let Some((key, text)) = map.next_entry::<String, Box<RawValue>>()? {
                    meta.insert_raw(key, Raw::from_text(text));
                }
                Ok(State(meta))
            }
        }
        d.deserialize_map(Read)
    }
}

/// Every field an assistant part of any kind takes. A field a kind may
/// leave out still must not be `null`, as the derived `Deserialize` reads
/// it.
#[derive(Default)]
struct Part {
    kind: Option<SmolStr>,
    text: Option<String>,
    redacted: Option<bool>,
    id: Option<SmolStr>,
    name: Option<SmolStr>,
    input: Option<Json>,
    invalid: Option<String>,
    provider_executed: Option<bool>,
    output: Option<OutputFields>,
    error: Option<bool>,
    media_type: Option<SmolStr>,
    source: Option<SourceFields>,
    meta: Option<State>,
}

impl<'de> Fields<'de> for Part {
    fn field<A: MapAccess<'de>>(
        &mut self,
        key: &str,
        map: &mut A,
    ) -> std::result::Result<bool, A::Error> {
        match key {
            "type" => self.kind = Some(next(map)?),
            "text" => self.text = Some(next(map)?),
            "redacted" => self.redacted = Some(next(map)?),
            "id" => self.id = next(map)?,
            "name" => self.name = next(map)?,
            "input" => self.input = next(map)?,
            "invalid" => self.invalid = next(map)?,
            "provider_executed" => self.provider_executed = Some(next(map)?),
            "output" => self.output = next(map)?,
            "error" => self.error = Some(next(map)?),
            "media_type" => self.media_type = next(map)?,
            "source" => self.source = next(map)?,
            "meta" => self.meta = Some(next(map)?),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl Part {
    fn text(self) -> Result<Text> {
        Ok(Text {
            text: required(self.text, "text")?,
            meta: self.meta.unwrap_or_default().0,
        })
    }
    fn tool_result(self) -> Result<ToolResult> {
        Ok(ToolResult {
            id: required(self.id, "id")?,
            name: required(self.name, "name")?,
            output: required(self.output, "output")?.build()?,
            error: self.error.unwrap_or(false),
            meta: self.meta.unwrap_or_default().0,
        })
    }
    fn build(self) -> Result<AssistantPart> {
        const KINDS: &[&str] = &[
            "text",
            "reasoning",
            "tool_call",
            "tool_result",
            "media",
            "opaque",
        ];
        let mut part = self;
        let kind = required(part.kind.take(), "type")?;
        if kind == "text" {
            return Ok(AssistantPart::Text(part.text()?));
        }
        if kind == "tool_result" {
            return Ok(AssistantPart::ToolResult(part.tool_result()?));
        }
        let Self {
            text,
            redacted,
            id,
            name,
            input,
            invalid,
            provider_executed,
            media_type,
            source,
            meta,
            ..
        } = part;
        let meta = meta.unwrap_or_default().0;
        Ok(match kind.as_str() {
            "reasoning" => AssistantPart::Reasoning(Reasoning {
                text: text.unwrap_or_default(),
                redacted: redacted.unwrap_or(false),
                meta,
            }),
            "tool_call" => AssistantPart::ToolCall(ToolCall {
                id: required(id, "id")?,
                name: required(name, "name")?,
                input: object(required(input, "input")?)?,
                invalid,
                provider_executed: provider_executed.unwrap_or(false),
                meta,
            }),
            "media" => AssistantPart::Media(Media {
                media_type: required(media_type, "media_type")?,
                source: required(source, "source")?.build()?,
                meta,
            }),
            "opaque" => AssistantPart::Opaque(Opaque { meta }),
            kind => return Err(unknown(kind, KINDS)),
        })
    }
}

/// An output's tag and its value as text, read once the tag is known.
#[derive(Default)]
struct OutputFields {
    kind: Option<SmolStr>,
    value: Option<Box<RawValue>>,
}

impl<'de> Fields<'de> for OutputFields {
    fn field<A: MapAccess<'de>>(
        &mut self,
        key: &str,
        map: &mut A,
    ) -> std::result::Result<bool, A::Error> {
        match key {
            "type" => self.kind = Some(next(map)?),
            "value" => self.value = Some(next(map)?),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl OutputFields {
    fn build(self) -> Result<Output> {
        const KINDS: &[&str] = &["text", "json", "content"];
        let kind = required(self.kind, "type")?;
        let value = required(self.value, "value")?;
        Ok(match kind.as_str() {
            "text" => Output::Text(serde_json::from_slice(value.get().as_bytes())?),
            "json" => Output::Json(Json::from(value)),
            "content" => {
                let parts: Vec<Part> = serde_json::from_slice(value.get().as_bytes())?;
                let mut content = Vec::new();
                for part in parts {
                    content.push(part.user()?);
                }
                Output::Content(content)
            }
            kind => return Err(unknown(kind, KINDS)),
        })
    }
}

impl Part {
    fn user(self) -> Result<UserPart> {
        let meta = self.meta.unwrap_or_default().0;
        Ok(match required(self.kind, "type")?.as_str() {
            "text" => UserPart::Text(Text {
                text: required(self.text, "text")?,
                meta,
            }),
            "media" => UserPart::Media(Media {
                media_type: required(self.media_type, "media_type")?,
                source: required(self.source, "source")?.build()?,
                meta,
            }),
            kind => return Err(unknown(kind, &["text", "media"])),
        })
    }
}

#[derive(Default)]
struct SourceFields {
    kind: Option<SmolStr>,
    data: Option<String>,
    url: Option<String>,
    id: Option<SmolStr>,
}

impl<'de> Fields<'de> for SourceFields {
    fn field<A: MapAccess<'de>>(
        &mut self,
        key: &str,
        map: &mut A,
    ) -> std::result::Result<bool, A::Error> {
        match key {
            "type" => self.kind = Some(next(map)?),
            "data" => self.data = next(map)?,
            "url" => self.url = next(map)?,
            "id" => self.id = next(map)?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl SourceFields {
    fn build(self) -> Result<Source> {
        Ok(match required(self.kind, "type")?.as_str() {
            "data" => Source::Data {
                data: required(self.data, "data")?,
            },
            "url" => Source::Url {
                url: required(self.url, "url")?,
            },
            "file" => Source::File {
                id: required(self.id, "id")?,
            },
            kind => return Err(unknown(kind, &["data", "url", "file"])),
        })
    }
}

#[cfg(test)]
mod tests;
