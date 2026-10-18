//! `GenerateContent` chunks into parts. Each part Gemini sends becomes a part
//! whose provider item is that part as sent. A text or thought part
//! continues the part before it while its kind stays the same, and a part
//! that carries only a thought signature joins the part beside it; any
//! other part stands alone. Only the first candidate is read.

use serde_json::{Value, json};

use super::KEY;
use crate::error::{Error, Kind};
use crate::json::Lenient;
use crate::protocol::{Decoder, Flow, Frame};
use crate::spec::{Event, Reason, Usage};
use crate::writer::{End, Open, Writer};

/// The fields of a part that carry no data of their own.
const fn is_bare(key: &str) -> bool {
    matches!(
        key.as_bytes(),
        b"thought" | b"thoughtSignature" | b"partMetadata"
    )
}

/// What a Google `finishReason` means. Every reason but `STOP` and
/// `MAX_TOKENS` is a failure: the content filters one kind, the rest
/// (`RECITATION`, `MALFORMED_FUNCTION_CALL`, …) another.
pub(crate) const fn reason_of(name: &str) -> Reason {
    match name.as_bytes() {
        b"STOP" => Reason::Stop,
        b"MAX_TOKENS" => Reason::Length,
        b"SAFETY"
        | b"BLOCKLIST"
        | b"PROHIBITED_CONTENT"
        | b"SPII"
        | b"IMAGE_SAFETY"
        | b"IMAGE_PROHIBITED_CONTENT"
        | b"MODEL_ARMOR" => Reason::ContentFilter,
        _ => Reason::Other,
    }
}

/// Usage from `usageMetadata`. Input is the prompt plus the tool-use
/// prompt, output the candidates plus the thoughts; a count Gemini leaves
/// out is zero, as its JSON omits zeros.
pub(crate) fn usage_of(usage: &Value) -> Usage {
    let count = |key: &str| usage.u64(key);
    let thoughts = count("thoughtsTokenCount");
    Usage {
        input: Some(
            count("promptTokenCount")
                .unwrap_or(0)
                .saturating_add(count("toolUsePromptTokenCount").unwrap_or(0)),
        ),
        output: Some(
            count("candidatesTokenCount")
                .unwrap_or(0)
                .saturating_add(thoughts.unwrap_or(0)),
        ),
        cache_read: count("cachedContentTokenCount"),
        cache_write: None,
        reasoning: thoughts,
    }
}

/// The error a blocked prompt's `promptFeedback` means; `None` when it
/// names no block. A verdict on the content is final; `OTHER` and reasons
/// no rule knows are transient.
pub(crate) fn blocked(feedback: &Value) -> Option<Error> {
    let reason = match feedback.get("blockReason")? {
        Value::String(reason) => reason.clone(),
        Value::Number(number) => format!("BLOCK_REASON_{number}"),
        _ => return None,
    };
    // The zero value: Gemini spells it `BLOCK_REASON_`, Vertex AI `BLOCKED_REASON_`.
    if matches!(
        reason.as_str(),
        "BLOCK_REASON_UNSPECIFIED" | "BLOCKED_REASON_UNSPECIFIED"
    ) {
        return None;
    }
    let spelled = |value: Option<&Value>| match value {
        Some(Value::String(name)) => name.clone(),
        Some(other) => other.to_string(),
        None => "<unset>".to_owned(),
    };
    let ratings: Vec<String> = feedback
        .arr("safetyRatings")
        .iter()
        .map(|rating| {
            format!(
                "{}={}",
                spelled(rating.get("category")),
                spelled(rating.get("probability"))
            )
        })
        .collect();
    let ratings = if ratings.is_empty() {
        String::new()
    } else {
        format!(", safety_ratings=[{}]", ratings.join(", "))
    };
    let message = format!("Gemini blocked the prompt: block_reason={reason}{ratings}");
    let mut error = Error::provider(Some(&reason), message);
    if matches!(
        reason.as_str(),
        "SAFETY"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "IMAGE_SAFETY"
            | "MODEL_ARMOR"
            | "JAILBREAK"
    ) {
        error.kind = Kind::ContentFilter;
    }
    Some(error)
}

/// The error an in-band `error` reports. A code that is an HTTP error
/// status classifies as that status would; any other is the provider's.
fn in_band(error: &Value) -> Error {
    let body = json!({ "error": error }).to_string();
    // A code spelled as a string is no status.
    let status = error
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .filter(|code| (400..=599).contains(code));
    if let Some(status) = status {
        return Error::from_status(status, &http::HeaderMap::new(), body.as_bytes());
    }
    let message = error
        .str("message")
        .map_or_else(|| body.clone(), str::to_owned);
    Error::provider(error.str("status"), message)
}

fn malformed(what: &str, data: &str) -> Error {
    Error::decode(format!("Gemini sent {what}"), data)
}

/// A streamed part merged into the part its block holds: the text appends,
/// a non-empty `thoughtSignature` replaces the one held, and any other
/// field replaces its namesake.
fn merge_part(item: &mut Value, part: &Value) {
    let (Value::Object(held), Value::Object(part)) = (&mut *item, part) else {
        *item = part.clone();
        return;
    };
    for (key, value) in part {
        if key == "text"
            && let (Some(Value::String(text)), Value::String(more)) = (held.get_mut("text"), value)
        {
            text.push_str(more);
        } else if key != "thoughtSignature" || value.as_str() != Some("") {
            held.insert(key.clone(), value.clone());
        }
    }
}

pub(crate) struct GeminiDecoder {
    w: Writer,
    /// The latest `finishReason`, and the `finishMessage` beside it.
    finish: Option<(String, Option<String>)>,
    /// The latest `usageMetadata`.
    usage: Option<Value>,
    model: Option<String>,
    response_id: Option<String>,
    /// The text or thought run later parts continue, and whether it is
    /// thought.
    run: Option<(usize, bool)>,
    /// Whether the run holds a signature.
    signed: bool,
    /// The part written last.
    last: Option<usize>,
    /// A signature sent alone before any part, which the next part takes.
    signature: Option<String>,
}

impl GeminiDecoder {
    pub(crate) fn new() -> Self {
        Self {
            // A call Gemini sends without an id (Gemini 2.5) keeps its part,
            // which takes the id replay gives it.
            w: Writer::new(KEY).with_call_id_slot(),
            finish: None,
            usage: None,
            model: None,
            response_id: None,
            run: None,
            signed: false,
            last: None,
            signature: None,
        }
    }

    fn decode(&mut self, data: &str) -> Result<Flow, Error> {
        let chunk: Value =
            serde_json::from_str(data).map_err(|error| Error::decode(error, data))?;
        if !chunk.is_object() {
            return Ok(Flow::More);
        }
        if let Some(id) = chunk.str("responseId").filter(|id| !id.is_empty()) {
            self.response_id = Some(id.to_owned());
        }
        if let Some(model) = chunk.str("modelVersion").filter(|model| !model.is_empty()) {
            self.model = Some(model.to_owned());
        }
        if let Some(usage) = chunk.get("usageMetadata") {
            self.usage = Some(usage.clone());
        }
        if let Some(error) = chunk.at("/error") {
            return Err(in_band(error));
        }
        if let Some(error) = chunk.get("promptFeedback").and_then(blocked) {
            return Err(error);
        }
        // The candidate, its content and its parts hold every part and the
        // finish, so a wrongly typed one fails the reply.
        let candidate = match chunk.get("candidates") {
            None | Some(Value::Null) => return Ok(Flow::More),
            Some(Value::Array(candidates)) => match candidates.first() {
                None => return Ok(Flow::More),
                Some(candidate @ Value::Object(_)) => candidate,
                Some(_) => return Err(malformed("a candidate that is not an object", data)),
            },
            Some(_) => return Err(malformed("candidates that are not a list", data)),
        };
        // The last reason wins: a hosted-tool round can report one before
        // the turn goes on. Proto3 JSON spells an enum value its schema does
        // not know as its number.
        let reason = match candidate.get("finishReason") {
            Some(Value::String(name)) => Some(name.clone()),
            Some(Value::Number(number)) => Some(format!("FINISH_REASON_{number}")),
            _ => None,
        };
        if let Some(reason) = &reason {
            let message = candidate.str("finishMessage").map(str::to_owned);
            self.finish = Some((reason.clone(), message));
        }
        let parts = match candidate.get("content") {
            None | Some(Value::Null) => None,
            Some(content @ Value::Object(_)) => content.get("parts"),
            Some(_) => return Err(malformed("candidate content that is not an object", data)),
        };
        match parts {
            None | Some(Value::Null) => {}
            Some(Value::Array(parts)) => {
                for part in parts {
                    self.part(part)?;
                }
            }
            Some(_) => return Err(malformed("candidate parts that are not a list", data)),
        }
        // A failure is final: nothing after it is read. A stop waits for the
        // end of the body.
        match reason.as_deref().map(reason_of) {
            None | Some(Reason::Stop | Reason::Length) => Ok(Flow::More),
            Some(_) => self.finish(),
        }
    }

    /// End the reply on the latest finish reason; without one it never
    /// ended.
    fn finish(&mut self) -> Result<Flow, Error> {
        let Some((name, message)) = self.finish.take() else {
            return Err(Error::incomplete());
        };
        self.close()?;
        let reason = reason_of(&name);
        self.w.end(End {
            usage: self.usage.as_ref().map_or_default(usage_of),
            reason: Some((reason, Some(name))),
            response_id: self.response_id.take(),
            model: self.model.take(),
            message: message.filter(|_| reason != Reason::Stop),
            meta: crate::Meta::new(),
        })
    }

    /// Finish the open run: Gemini states each part whole.
    fn close(&mut self) -> Result<(), Error> {
        match self.run.take() {
            Some((index, _)) => self.w.finish(index),
            None => Ok(()),
        }
    }

    /// Write a part at a fresh index, closing the run before it. Text and
    /// thought stay open as the run later parts continue; any other part is
    /// whole.
    fn open(&mut self, block: Open, mut item: Value, text: &str) -> Result<(), Error> {
        self.close()?;
        let index = self.w.fresh_index();
        if let (Some(signature), Some(fields)) = (self.signature.take(), item.as_object_mut()) {
            fields
                .entry("thoughtSignature")
                .or_insert(Value::String(signature));
        }
        self.last = Some(index);
        self.signed = item
            .str("thoughtSignature")
            .is_some_and(|signature| !signature.is_empty());
        let run = match block {
            Open::Text => Some(false),
            Open::Reasoning { .. } => Some(true),
            Open::Call { .. } | Open::Opaque { .. } | Open::Media(_) => None,
        };
        self.w.open(index, block, item)?;
        self.w.push(index, text)?;
        self.run = run.map(|thought| (index, thought));
        if self.run.is_none() {
            self.w.finish(index)?;
        }
        Ok(())
    }

    /// One part of the candidate's content. A part with no model here
    /// (inline data, code and its result, a kind Gemini adds later) is an
    /// opaque part that replays; one with no data, or that is not an object,
    /// is kept as one that does not.
    fn part(&mut self, part: &Value) -> Result<(), Error> {
        let Value::Object(fields) = part else {
            return self.open(Open::Opaque { replay: false }, part.clone(), "");
        };
        let thought = part.bool("thought") == Some(true);
        let signature = part
            .str("thoughtSignature")
            .filter(|signature| !signature.is_empty());
        if let Some(text) = part.str("text") {
            // An empty part without a signature carries nothing.
            if text.is_empty() && signature.is_none() {
                return Ok(());
            }
            // A second signed part starts a part of its own, so each
            // signature stays with the part that carried it.
            return match self.run {
                Some((index, kind)) if kind == thought && !(self.signed && signature.is_some()) => {
                    self.signed |= signature.is_some();
                    self.w.push(index, text)?;
                    self.w.edit(index, |item| merge_part(item, part))
                }
                _ if thought => self.open(Open::Reasoning { redacted: false }, part.clone(), text),
                _ => self.open(Open::Text, part.clone(), text),
            };
        }
        if let Some(call) = part.obj("functionCall") {
            // Nothing can answer a call with no name.
            let Some(name) = call
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            else {
                return Ok(());
            };
            // Proto3 JSON leaves out an empty `args` struct.
            let arguments = call
                .get("args")
                .map_or_else(|| "{}".to_owned(), Value::to_string);
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
            let block = Open::Call {
                id: id.to_owned(),
                name: name.to_owned(),
            };
            return self.open(block, part.clone(), &arguments);
        }
        let data = fields.keys().any(|key| !is_bare(key));
        if !data && let Some(signature) = signature {
            // Gemini hangs a signature on the part before it, or the part
            // after when none came yet.
            let Some(index) = self.run.map(|(index, _)| index).or(self.last) else {
                self.signature = Some(signature.to_owned());
                return Ok(());
            };
            return self.w.edit(index, |item| {
                if let Some(item) = item.as_object_mut() {
                    item.insert("thoughtSignature".to_owned(), Value::from(signature));
                }
            });
        }
        self.open(Open::Opaque { replay: data }, part.clone(), "")
    }
}

impl Decoder for GeminiDecoder {
    fn frame(&mut self, frame: Frame<'_>, out: &mut Vec<Event>) -> Result<Flow, Error> {
        let flow = self.decode(frame.data);
        self.w.drain(out);
        flow
    }

    /// Gemini ends a reply at the end of its body, not at its first finish
    /// reason.
    fn end(&mut self, out: &mut Vec<Event>) -> Result<(), Error> {
        let flow = self.finish();
        self.w.drain(out);
        flow.map(drop)
    }
}
