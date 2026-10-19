//! [`Response::from_value`] by hand, reading what the derived
//! `Deserialize` reads. Derived, the internally tagged parts buffer each
//! part's fields in serde's `Content` and copy its strings twice; that code
//! cost ~50 KiB, and a session reads every logged reply again on each turn.

use serde::de::Error as _;
use serde_json::{Error, Map, Value};

use super::{
    Assistant, AssistantPart, Finish, Media, Meta, Opaque, Origin, Output, Reason, Reasoning,
    Response, SmolStr, Source, Text, ToolCall, ToolResult, Usage, UserPart,
};

type Result<T> = std::result::Result<T, Error>;

fn invalid(value: &Value, expected: &str) -> Error {
    Error::custom(format_args!("invalid type: {value}, expected {expected}"))
}

/// The fields of an object.
#[derive(Clone, Copy)]
struct Fields<'a>(&'a Map<String, Value>);

impl<'a> Fields<'a> {
    fn of(value: &'a Value) -> Result<Self> {
        value
            .as_object()
            .map(Self)
            .ok_or_else(|| invalid(value, "an object"))
    }

    fn required(self, key: &'static str) -> Result<&'a Value> {
        self.0.get(key).ok_or_else(|| Error::missing_field(key))
    }

    /// The field, `None` when absent or `null`.
    fn optional(self, key: &str) -> Option<&'a Value> {
        self.0.get(key).filter(|value| !value.is_null())
    }

    fn str(self, key: &'static str) -> Result<&'a str> {
        let value = self.required(key)?;
        value.as_str().ok_or_else(|| invalid(value, "a string"))
    }

    fn opt_str(self, key: &str) -> Result<Option<&'a str>> {
        self.optional(key)
            .map(|value| value.as_str().ok_or_else(|| invalid(value, "a string")))
            .transpose()
    }

    fn smol(self, key: &'static str) -> Result<SmolStr> {
        self.str(key).map(SmolStr::from)
    }

    fn opt_smol(self, key: &str) -> Result<Option<SmolStr>> {
        Ok(self.opt_str(key)?.map(SmolStr::from))
    }

    fn opt_string(self, key: &str) -> Result<Option<String>> {
        Ok(self.opt_str(key)?.map(str::to_owned))
    }

    /// A flag that defaults to `false`.
    fn flag(self, key: &str) -> Result<bool> {
        match self.0.get(key) {
            None => Ok(false),
            Some(value) => value.as_bool().ok_or_else(|| invalid(value, "a boolean")),
        }
    }

    fn map(value: &Value) -> Result<Map<String, Value>> {
        value
            .as_object()
            .cloned()
            .ok_or_else(|| invalid(value, "a map"))
    }

    /// The part state, empty when absent.
    fn meta(self) -> Result<Meta> {
        let Some(value) = self.0.get("meta") else {
            return Ok(Meta::new());
        };
        let map = value.as_object().ok_or_else(|| invalid(value, "a map"))?;
        // Inserted, not collected: collecting sorts first, in ~5 KiB of code.
        let mut meta = Meta::new();
        meta.extend(map.iter().map(|(key, value)| (key.clone(), value.clone())));
        Ok(meta)
    }

    fn list<T>(self, key: &'static str, item: fn(&Value) -> Result<T>) -> Result<Vec<T>> {
        let value = self.required(key)?;
        let items = value
            .as_array()
            .ok_or_else(|| invalid(value, "a sequence"))?;
        items.iter().map(item).collect()
    }
}

fn unknown(kind: &str) -> Error {
    Error::custom(format_args!("unknown variant `{kind}`"))
}

impl Response {
    /// A recorded reply, as the derived `Deserialize` reads it.
    pub fn from_value(value: &Value) -> serde_json::Result<Self> {
        let fields = Fields::of(value)?;
        Ok(Self {
            message: assistant(fields.required("message")?)?,
            finish: finish(fields.required("finish")?)?,
            usage: usage(fields.required("usage")?)?,
            id: fields.opt_smol("id")?,
            model: fields.opt_smol("model")?,
            meta: fields.meta()?,
        })
    }
}

fn assistant(value: &Value) -> Result<Assistant> {
    let fields = Fields::of(value)?;
    let origin = match fields.optional("origin") {
        None => None,
        Some(origin) => {
            let origin = Fields::of(origin)?;
            Some(Origin {
                protocol: origin.smol("protocol")?,
                provider: origin.smol("provider")?,
                model: origin.smol("model")?,
            })
        }
    };
    Ok(Assistant {
        content: fields.list("content", assistant_part)?,
        origin,
    })
}

fn finish(value: &Value) -> Result<Finish> {
    let fields = Fields::of(value)?;
    let reason = match fields.str("reason")? {
        "stop" => Reason::Stop,
        "length" => Reason::Length,
        "tool_calls" => Reason::ToolCalls,
        "content_filter" => Reason::ContentFilter,
        "error" => Reason::Error,
        "other" => Reason::Other,
        reason => return Err(unknown(reason)),
    };
    Ok(Finish {
        reason,
        raw: fields.opt_smol("raw")?,
        message: fields.opt_string("message")?,
    })
}

fn usage(value: &Value) -> Result<Usage> {
    let fields = Fields::of(value)?;
    let count = |key: &str| {
        fields
            .optional(key)
            .map(|value| value.as_u64().ok_or_else(|| invalid(value, "u64")))
            .transpose()
    };
    Ok(Usage {
        input: count("input")?,
        output: count("output")?,
        cache_read: count("cache_read")?,
        cache_write: count("cache_write")?,
        cache_write_long: count("cache_write_long")?,
        tool_input: count("tool_input")?,
        audio_input: count("audio_input")?,
        reasoning: count("reasoning")?,
        audio_output: count("audio_output")?,
        hosted_tool_calls: count("hosted_tool_calls")?,
    })
}

fn text(fields: Fields<'_>) -> Result<Text> {
    Ok(Text {
        text: fields.str("text")?.to_owned(),
        meta: fields.meta()?,
    })
}

fn media(fields: Fields<'_>) -> Result<Media> {
    let source = Fields::of(fields.required("source")?)?;
    let source = match source.str("type")? {
        "data" => Source::Data {
            data: source.str("data")?.to_owned(),
        },
        "url" => Source::Url {
            url: source.str("url")?.to_owned(),
        },
        "file" => Source::File {
            id: source.smol("id")?,
        },
        kind => return Err(unknown(kind)),
    };
    Ok(Media {
        media_type: fields.smol("media_type")?,
        source,
        meta: fields.meta()?,
    })
}

fn user_part(value: &Value) -> Result<UserPart> {
    let fields = Fields::of(value)?;
    match fields.str("type")? {
        "text" => text(fields).map(UserPart::Text),
        "media" => media(fields).map(UserPart::Media),
        kind => Err(unknown(kind)),
    }
}

fn output(value: &Value) -> Result<Output> {
    let fields = Fields::of(value)?;
    let kind = fields.str("type")?;
    let value = fields.required("value")?;
    Ok(match kind {
        "text" => Output::Text(
            value
                .as_str()
                .ok_or_else(|| invalid(value, "a string"))?
                .to_owned(),
        ),
        "json" => Output::Json(value.clone()),
        "content" => {
            let parts = value
                .as_array()
                .ok_or_else(|| invalid(value, "a sequence"))?;
            Output::Content(parts.iter().map(user_part).collect::<Result<_>>()?)
        }
        kind => return Err(unknown(kind)),
    })
}

fn assistant_part(value: &Value) -> Result<AssistantPart> {
    let fields = Fields::of(value)?;
    Ok(match fields.str("type")? {
        "text" => AssistantPart::Text(text(fields)?),
        "reasoning" => AssistantPart::Reasoning(Reasoning {
            text: match fields.0.get("text") {
                None => String::new(),
                Some(_) => fields.str("text")?.to_owned(),
            },
            redacted: fields.flag("redacted")?,
            meta: fields.meta()?,
        }),
        "tool_call" => AssistantPart::ToolCall(ToolCall {
            id: fields.smol("id")?,
            name: fields.smol("name")?,
            input: Fields::map(fields.required("input")?)?,
            invalid: fields.opt_string("invalid")?,
            provider_executed: fields.flag("provider_executed")?,
            meta: fields.meta()?,
        }),
        "tool_result" => AssistantPart::ToolResult(ToolResult {
            id: fields.smol("id")?,
            name: fields.smol("name")?,
            output: output(fields.required("output")?)?,
            error: fields.flag("error")?,
            meta: fields.meta()?,
        }),
        "media" => AssistantPart::Media(media(fields)?),
        "opaque" => AssistantPart::Opaque(Opaque {
            meta: fields.meta()?,
        }),
        kind => return Err(unknown(kind)),
    })
}

#[cfg(test)]
mod tests;
