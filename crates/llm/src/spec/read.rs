//! [`Response::from_json`]: a logged reply read from its text, each field
//! once. The derived `Deserialize` buffers each internally tagged part in
//! serde's `Content`, copying its strings twice (that code cost ~50 KiB),
//! and cannot keep provider state as the text it is. Here a part is one
//! flat struct of every kind's fields, its `type` choosing which it takes,
//! and state stays the text the log holds. A field reads as the derived
//! `Deserialize` reads it for the kind that takes it; a field of another
//! kind must still be of its own type.

use std::fmt;

use serde::de::{Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use serde_json::{Error, Map, Value};

use super::meta::Raw;
use super::{
    Assistant, AssistantPart, Finish, Media, Meta, Opaque, Origin, Output, Reasoning, Response,
    SmolStr, Source, Text, ToolCall, ToolResult, Usage, UserPart,
};

type Result<T> = std::result::Result<T, Error>;

impl Response {
    /// A reply as this crate writes it, read from its text.
    pub fn from_json(text: &str) -> serde_json::Result<Self> {
        let Reply {
            message,
            finish,
            usage,
            id,
            model,
            meta,
        } = serde_json::from_str(text)?;
        Ok(Self {
            message: Assistant {
                content: message.content.0,
                origin: message.origin,
            },
            finish,
            usage,
            id,
            model,
            meta: meta.0,
        })
    }
}

#[derive(Deserialize)]
struct Reply {
    message: Message,
    finish: Finish,
    usage: Usage,
    id: Option<SmolStr>,
    model: Option<SmolStr>,
    #[serde(default)]
    meta: State,
}

#[derive(Deserialize)]
struct Message {
    content: Content,
    origin: Option<Origin>,
}

/// A message's parts, each built as it is read, in a list of their number.
struct Content(Vec<AssistantPart>);

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Read;
        impl<'de> Visitor<'de> for Read {
            type Value = Content;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a sequence")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Content, A::Error> {
                let mut parts = Vec::new();
                while let Some(part) = seq.next_element::<Part<'de>>()? {
                    parts.push(part.build().map_err(A::Error::custom)?);
                }
                parts.shrink_to_fit();
                Ok(Content(parts))
            }
        }
        d.deserialize_seq(Read)
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

/// A field that defaults when absent, but not when `null`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

fn required<T>(field: Option<T>, key: &'static str) -> Result<T> {
    field.ok_or_else(|| Error::missing_field(key))
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PartKind {
    Text,
    Reasoning,
    ToolCall,
    ToolResult,
    Media,
    Opaque,
}

/// Every field an assistant part of any kind takes.
#[derive(Deserialize)]
struct Part<'a> {
    #[serde(rename = "type")]
    kind: PartKind,
    #[serde(default, deserialize_with = "present")]
    text: Option<String>,
    #[serde(default, deserialize_with = "present")]
    redacted: Option<bool>,
    id: Option<SmolStr>,
    name: Option<SmolStr>,
    input: Option<Map<String, Value>>,
    invalid: Option<String>,
    #[serde(default, deserialize_with = "present")]
    provider_executed: Option<bool>,
    #[serde(borrow)]
    output: Option<OutputFields<'a>>,
    #[serde(default, deserialize_with = "present")]
    error: Option<bool>,
    media_type: Option<SmolStr>,
    source: Option<SourceFields>,
    #[serde(default, deserialize_with = "present")]
    meta: Option<State>,
}

impl Part<'_> {
    fn build(self) -> Result<AssistantPart> {
        let meta = self.meta.unwrap_or_default().0;
        Ok(match self.kind {
            PartKind::Text => AssistantPart::Text(Text {
                text: required(self.text, "text")?,
                meta,
            }),
            PartKind::Reasoning => AssistantPart::Reasoning(Reasoning {
                text: self.text.unwrap_or_default(),
                redacted: self.redacted.unwrap_or(false),
                meta,
            }),
            PartKind::ToolCall => AssistantPart::ToolCall(ToolCall {
                id: required(self.id, "id")?,
                name: required(self.name, "name")?,
                input: required(self.input, "input")?,
                invalid: self.invalid,
                provider_executed: self.provider_executed.unwrap_or(false),
                meta,
            }),
            PartKind::ToolResult => AssistantPart::ToolResult(ToolResult {
                id: required(self.id, "id")?,
                name: required(self.name, "name")?,
                output: required(self.output, "output")?.build()?,
                error: self.error.unwrap_or(false),
                meta,
            }),
            PartKind::Media => AssistantPart::Media(Media {
                media_type: required(self.media_type, "media_type")?,
                source: required(self.source, "source")?.build()?,
                meta,
            }),
            PartKind::Opaque => AssistantPart::Opaque(Opaque { meta }),
        })
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutputKind {
    Text,
    Json,
    Content,
}

/// An output's tag and its value as text, read once the tag is known.
#[derive(Deserialize)]
struct OutputFields<'a> {
    #[serde(rename = "type")]
    kind: OutputKind,
    #[serde(default, borrow, deserialize_with = "present")]
    value: Option<&'a RawValue>,
}

impl OutputFields<'_> {
    fn build(self) -> Result<Output> {
        let value = required(self.value, "value")?.get();
        Ok(match self.kind {
            OutputKind::Text => Output::Text(serde_json::from_str(value)?),
            OutputKind::Json => Output::Json(serde_json::from_str(value)?),
            OutputKind::Content => {
                let parts: Vec<UserFields> = serde_json::from_str(value)?;
                let parts = parts.into_iter().map(UserFields::build);
                let mut parts = parts.collect::<Result<Vec<_>>>()?;
                parts.shrink_to_fit();
                Output::Content(parts)
            }
        })
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UserKind {
    Text,
    Media,
}

#[derive(Deserialize)]
struct UserFields {
    #[serde(rename = "type")]
    kind: UserKind,
    text: Option<String>,
    media_type: Option<SmolStr>,
    source: Option<SourceFields>,
    #[serde(default, deserialize_with = "present")]
    meta: Option<State>,
}

impl UserFields {
    fn build(self) -> Result<UserPart> {
        let meta = self.meta.unwrap_or_default().0;
        Ok(match self.kind {
            UserKind::Text => UserPart::Text(Text {
                text: required(self.text, "text")?,
                meta,
            }),
            UserKind::Media => UserPart::Media(Media {
                media_type: required(self.media_type, "media_type")?,
                source: required(self.source, "source")?.build()?,
                meta,
            }),
        })
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SourceKind {
    Data,
    Url,
    File,
}

#[derive(Deserialize)]
struct SourceFields {
    #[serde(rename = "type")]
    kind: SourceKind,
    data: Option<String>,
    url: Option<String>,
    id: Option<SmolStr>,
}

impl SourceFields {
    fn build(self) -> Result<Source> {
        Ok(match self.kind {
            SourceKind::Data => Source::Data {
                data: required(self.data, "data")?,
            },
            SourceKind::Url => Source::Url {
                url: required(self.url, "url")?,
            },
            SourceKind::File => Source::File {
                id: required(self.id, "id")?,
            },
        })
    }
}

#[cfg(test)]
mod tests;
