//! `GenerateContent` chunks into parts. Each part Gemini sends becomes a part
//! whose provider item is that part as sent. A text or thought part
//! continues the part before it while its kind stays the same, and a part
//! that carries only a thought signature joins the part beside it; any
//! other part stands alone. Only the first candidate is read.
//!
//! Chunks parse into types that borrow their text, so streamed text costs
//! only the copies its events keep. They read as [`Lenient`] does: a field
//! of a type Gemini's schema does not give it is absent.

use std::borrow::Cow;
use std::fmt::Write as _;

use serde::de::MapAccess;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use smol_str::format_smolstr;

use super::KEY;
use crate::error::{Error, Kind};
use crate::json::{Fields, Json, Lenient, fields, first, present};
use crate::protocol::Flow;
use crate::spec::{Media, Meta, Reason, SmolStr, Source, Usage};
use crate::writer::{End, Open, Writer, Writes};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Chunk<'a> {
    #[serde(borrow, default)]
    response_id: Json<'a>,
    #[serde(borrow, default)]
    model_version: Json<'a>,
    #[serde(default, deserialize_with = "present")]
    usage_metadata: bool,
    error: Option<Value>,
    prompt_feedback: Option<Value>,
    /// The first: it holds every part and the finish, so one of the wrong
    /// type, or with content or parts of the wrong type, fails the reply.
    #[serde(borrow, default, deserialize_with = "first")]
    candidates: Option<Candidate<'a>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate<'a> {
    /// A name, or as proto3 JSON spells one its schema does not know, a
    /// number.
    #[serde(borrow, default)]
    finish_reason: Json<'a>,
    #[serde(borrow, default)]
    finish_message: Json<'a>,
    #[serde(borrow)]
    content: Option<Content<'a>>,
}

#[derive(Deserialize)]
struct Content<'a> {
    #[serde(borrow)]
    parts: Option<Vec<Json<'a, Part<'a>>>>,
}

/// An object part: its text, borrowed unless it holds escapes, when that
/// is a string, and its other fields as sent.
#[derive(Default)]
struct Part<'a> {
    text: Option<Cow<'a, str>>,
    rest: Map<String, Value>,
}

impl<'de: 'a, 'a> Fields<'de> for Part<'a> {
    fn field<A: MapAccess<'de>>(&mut self, key: &str, map: &mut A) -> Result<bool, A::Error> {
        if key != "text" {
            return Ok(false);
        }
        match map.next_value::<Json<'a>>()? {
            Json::Str(text) => self.text = Some(text),
            other => {
                let value = other.into_value().unwrap_or_default();
                self.rest.insert(key.to_owned(), value);
            }
        }
        Ok(true)
    }

    fn rest(&mut self) -> Option<&mut Map<String, Value>> {
        Some(&mut self.rest)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Part<'a> {
    fn deserialize<D: Deserializer<'de>>(json: D) -> Result<Self, D::Error> {
        fields(json)
    }
}

/// A chunk's usage, the rest unread.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageOf {
    usage_metadata: Value,
}

/// Whether a finish or block reason is a content filter's verdict, final.
fn filtered(reason: &str) -> bool {
    matches!(
        reason,
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "IMAGE_SAFETY" | "MODEL_ARMOR"
    )
}

/// Every finish reason but `STOP` and `MAX_TOKENS` is a failure: the
/// content filters' are one kind, the rest (`RECITATION`,
/// `MALFORMED_FUNCTION_CALL`, …) another.
fn reason_of(name: &str) -> Reason {
    match name {
        "STOP" => Reason::Stop,
        "MAX_TOKENS" => Reason::Length,
        "SPII" | "IMAGE_PROHIBITED_CONTENT" => Reason::ContentFilter,
        name if filtered(name) => Reason::ContentFilter,
        _ => Reason::Other,
    }
}

/// Usage from `usageMetadata`. Input is the prompt plus the tool-use
/// prompt, output the candidates plus the thoughts; a count Gemini leaves
/// out is zero, as its JSON omits zeros. Audio is read from the per-modality
/// details.
fn usage_of(usage: &Value) -> Usage {
    let count = |key: &str| usage.u64(key);
    let is_audio = |detail: &&Value| detail.str("modality") == Some("AUDIO");
    let audio = |key: &str| {
        let all = usage.arr(key).iter().filter(is_audio);
        all.filter_map(|d| d.u64("tokenCount")).reduce(|a, b| a + b)
    };
    let thoughts = count("thoughtsTokenCount");
    let tool = count("toolUsePromptTokenCount");
    Usage {
        input: Some(count("promptTokenCount").unwrap_or(0) + tool.unwrap_or(0)),
        output: Some(count("candidatesTokenCount").unwrap_or(0) + thoughts.unwrap_or(0)),
        cache_read: count("cachedContentTokenCount"),
        tool_input: tool,
        audio_input: audio("promptTokensDetails"),
        reasoning: thoughts,
        audio_output: audio("candidatesTokensDetails"),
        ..Usage::default()
    }
}

/// The error a blocked prompt's `promptFeedback` means; `None` when it
/// names no block. A verdict on the content is final; `OTHER` and reasons
/// no rule knows are transient.
fn blocked(feedback: &Value) -> Option<Error> {
    let reason = match feedback.get("blockReason")? {
        Value::String(reason) => reason.clone(),
        Value::Number(number) => format!("BLOCK_REASON_{number}"),
        _ => return None,
    };
    // The zero value: Gemini spells it `BLOCK_REASON_`, Vertex AI `BLOCKED_REASON_`.
    if reason == "BLOCK_REASON_UNSPECIFIED" || reason == "BLOCKED_REASON_UNSPECIFIED" {
        return None;
    }
    let spelled = |value: Option<&Value>| match value {
        Some(Value::String(name)) => name.clone(),
        Some(other) => other.to_string(),
        None => "<unset>".to_owned(),
    };
    let ratings = feedback.arr("safetyRatings").iter().map(|rating| {
        let category = spelled(rating.get("category"));
        format!("{category}={}", spelled(rating.get("probability")))
    });
    let ratings = ratings.collect::<Vec<_>>().join(", ");
    let mut message = format!("Gemini blocked the prompt: block_reason={reason}");
    if !ratings.is_empty() {
        let _ = write!(message, ", safety_ratings=[{ratings}]");
    }
    let mut error = Error::provider(Some(&reason), message);
    if filtered(&reason) || reason == "JAILBREAK" {
        error.kind = Kind::ContentFilter;
    }
    Some(error)
}

/// The error an in-band `error` reports. A code that is an HTTP error
/// status classifies as that status would; any other, or one spelled as a
/// string, is the provider's.
fn in_band(error: &Value) -> Error {
    let body = json!({ "error": error }).to_string();
    let code = error.get("code").and_then(Value::as_u64);
    let status = code.and_then(|code| u16::try_from(code).ok());
    if let Some(status) = status.filter(|status| (400..=599).contains(status)) {
        return Error::from_status(status, &http::HeaderMap::new(), body.as_bytes());
    }
    let message = error.str("message").map_or(body, str::to_owned);
    Error::provider(error.str("status"), message)
}

/// The image (or other media) a reply part carries inline or by URI.
fn media_of(part: &Map<String, Value>) -> Option<Media> {
    let inline = part.get("inlineData").filter(|inline| inline.is_object());
    let (media_type, source) = if let Some(inline) = inline {
        let data = inline.str("data")?.to_owned();
        (inline.str("mimeType")?, Source::Data { data })
    } else {
        let file = part.get("fileData")?;
        let url = file.str("fileUri")?.to_owned();
        let media_type = file.str("mimeType").unwrap_or("application/octet-stream");
        (media_type, Source::Url { url })
    };
    let media_type = media_type.into();
    Some(Media {
        media_type,
        source,
        meta: Meta::new(),
    })
}

pub(crate) struct GeminiDecoder {
    w: Writer,
    /// What the reply's end reports, as far as it is known: the latest
    /// `finishReason` and the `finishMessage` beside it, the reply's id and
    /// model.
    end: End,
    /// The latest chunk that carried `usageMetadata`, read when the reply
    /// ends: most chunks carry one.
    usage: String,
    /// The part written last.
    last: Option<usize>,
    /// Whether that part is a run later text continues, and if so whether
    /// thought.
    run: Option<bool>,
    /// Whether the run holds a signature.
    signed: bool,
    /// A signature sent alone before any part, which the next part takes.
    signature: Option<Value>,
}

impl GeminiDecoder {
    pub(crate) fn new() -> Self {
        Self {
            // A call Gemini sends without an id (Gemini 2.5) keeps its part,
            // which takes the id replay gives it.
            w: Writer::new(KEY).with_call_id_slot(),
            end: End::default(),
            usage: String::new(),
            last: None,
            run: None,
            signed: false,
            signature: None,
        }
    }

    /// End the reply on the latest finish reason; without one it never
    /// ended.
    fn finish(&mut self) -> Result<Flow, Error> {
        let mut end = std::mem::take(&mut self.end);
        let Some((reason, _)) = end.reason else {
            return Err(Error::incomplete());
        };
        self.close()?;
        let chunk = serde_json::from_str::<UsageOf>(&self.usage).ok();
        end.raw_usage = chunk.map(|chunk| chunk.usage_metadata);
        end.usage = end.raw_usage.as_ref().map_or_default(usage_of);
        end.message = end.message.filter(|_| reason != Reason::Stop);
        self.w.end(end)
    }

    /// Finish the open run, its item holding the text it streamed: Gemini
    /// states each part whole.
    fn close(&mut self) -> Result<(), Error> {
        let (Some(_), Some(index)) = (self.run.take(), self.last) else {
            return Ok(());
        };
        let text = self.w.text(index).unwrap_or_default().to_owned();
        self.w.edit(index, |item| {
            if let Some(item) = item.as_object_mut() {
                item.insert("text".into(), text.into());
            }
        })?;
        self.w.finish(index)
    }

    /// Write a part at a fresh index, closing the run before it. Text and
    /// thought stay open as the run later parts continue; any other part is
    /// whole.
    fn open<'a>(
        &mut self,
        block: Open,
        mut item: Value,
        text: impl Into<Cow<'a, str>>,
    ) -> Result<(), Error> {
        self.close()?;
        let index = self.w.fresh_index();
        if let (Some(signature), Some(fields)) = (self.signature.take(), item.as_object_mut()) {
            fields.entry("thoughtSignature").or_insert(signature);
        }
        self.last = Some(index);
        self.signed = item.str("thoughtSignature").is_some_and(|s| !s.is_empty());
        self.run = match block {
            Open::Text => Some(false),
            Open::Reasoning { .. } => Some(true),
            Open::Call { .. } | Open::Opaque { .. } | Open::Media(_) => {
                return self.w.whole(index, block, item, text);
            }
        };
        self.w.open(index, block, item)?;
        self.w.push(index, text)
    }

    /// One part of the candidate's content. A part with no model here
    /// (code and its result, a kind Gemini adds later) is an opaque part
    /// that replays; one with no data, or that is not an object, is kept
    /// as one that does not.
    fn part(&mut self, part: Json<'_, Part<'_>>) -> Result<(), Error> {
        let Part { text, mut rest } = match part {
            Json::Object(part) => part,
            stray => {
                let stray = stray.into_value().unwrap_or_default();
                return self.open(Open::Opaque { replay: false }, stray, "");
            }
        };
        let thought = rest.get("thought").and_then(Value::as_bool) == Some(true);
        let signature = rest.get("thoughtSignature").and_then(Value::as_str);
        let signed = signature.is_some_and(|signature| !signature.is_empty());
        if let Some(text) = text {
            // An empty part without a signature carries nothing.
            if text.is_empty() && !signed {
                return Ok(());
            }
            // A second signed part starts a part of its own, so each
            // signature stays with the part that carried it.
            if let (Some(index), Some(run)) = (self.last, self.run)
                && run == thought
                && !(self.signed && signed)
            {
                self.signed |= signed;
                self.w.push(index, text)?;
                // A non-empty signature replaces the one held, and any other
                // field replaces its namesake.
                let kept =
                    |(key, value): &(String, Value)| key != "thoughtSignature" || value != "";
                let rest = rest.into_iter().filter(kept);
                return self.w.edit(index, |item| {
                    if let Some(item) = item.as_object_mut() {
                        item.extend(rest);
                    }
                });
            }
            let reasoning = Open::Reasoning { redacted: false };
            let block = if thought { reasoning } else { Open::Text };
            return self.open(block, Value::Object(rest), text);
        }
        if let Some(call) = rest.get("functionCall").and_then(Value::as_object) {
            let field = |key| call.get(key).and_then(Value::as_str).unwrap_or_default();
            // Nothing can answer a call with no name.
            if field("name").is_empty() {
                return Ok(());
            }
            let (id, name) = (field("id").into(), field("name").into());
            // Proto3 JSON leaves out an empty `args` struct.
            let arguments = call
                .get("args")
                .map_or_else(|| "{}".to_owned(), Value::to_string);
            return self.open(Open::Call { id, name }, Value::Object(rest), arguments);
        }
        let bare = ["thought", "thoughtSignature", "partMetadata"];
        let data = rest.keys().any(|key| !bare.contains(&key.as_str()));
        if !data && signed {
            // Gemini hangs a signature on the part before it, or the part
            // after when none came yet.
            let signature = rest.remove("thoughtSignature");
            let Some(index) = self.last else {
                self.signature = signature;
                return Ok(());
            };
            return self.w.edit(index, |item| {
                if let (Some(item), Some(signature)) = (item.as_object_mut(), signature) {
                    item.insert("thoughtSignature".into(), signature);
                }
            });
        }
        let block = media_of(&rest).map_or(Open::Opaque { replay: data }, Open::Media);
        self.open(block, Value::Object(rest), "")
    }
}

impl Writes for GeminiDecoder {
    fn writer(&mut self) -> &mut Writer {
        &mut self.w
    }

    fn data(&mut self, data: &str) -> Result<Flow, Error> {
        let chunk = match serde_json::from_str::<Chunk<'_>>(data) {
            Ok(chunk) => chunk,
            // The chunk as a value tells what went wrong.
            Err(shape) => {
                let chunk: Value =
                    serde_json::from_str(data).map_err(|error| Error::decode(error, data))?;
                if !chunk.is_object() {
                    return Ok(Flow::More);
                }
                if let Some(error) = chunk.at("/error") {
                    return Err(in_band(error));
                }
                let shape = || Error::decode(format!("Gemini sent {shape}"), data);
                let blocked = chunk.get("promptFeedback").and_then(blocked);
                return Err(blocked.unwrap_or_else(shape));
            }
        };
        if let Some(id) = chunk.response_id.str().filter(|id| !id.is_empty()) {
            self.end.response_id = Some(id.into());
        }
        if let Some(model) = chunk.model_version.str().filter(|model| !model.is_empty()) {
            self.end.model = Some(model.into());
        }
        if chunk.usage_metadata {
            data.clone_into(&mut self.usage);
        }
        if let Some(error) = chunk.error {
            return Err(in_band(&error));
        }
        if let Some(error) = chunk.prompt_feedback.as_ref().and_then(blocked) {
            return Err(error);
        }
        let Some(candidate) = chunk.candidates else {
            return Ok(Flow::More);
        };
        // The last reason wins: a hosted-tool round can report one before
        // the turn goes on.
        let name = match candidate.finish_reason {
            Json::Str(name) => Some(SmolStr::from(name)),
            Json::Other(Value::Number(number)) => Some(format_smolstr!("FINISH_REASON_{number}")),
            _ => None,
        };
        if let Some(name) = name {
            self.end.reason = Some((reason_of(&name), Some(name)));
            self.end.message = candidate.finish_message.str().map(str::to_owned);
        }
        let parts = candidate.content.and_then(|content| content.parts);
        for part in parts.into_iter().flatten() {
            self.part(part)?;
        }
        // A failure is final: nothing after it is read. A stop waits for the
        // end of the body.
        match self.end.reason.as_ref().map(|(reason, _)| *reason) {
            None | Some(Reason::Stop | Reason::Length) => Ok(Flow::More),
            Some(_) => self.finish(),
        }
    }

    /// Gemini ends a reply at the end of its body, not at its first finish
    /// reason.
    fn ended(&mut self) -> Result<(), Error> {
        self.finish().map(drop)
    }
}
