//! The one pass every request goes through before a protocol encodes it:
//! what another origin produced is cut down to what this target can take,
//! and the transcript is repaired into a shape every provider accepts.
//!
//! - An assistant message from another protocol, provider or model loses
//!   its reasoning, its opaque items, its hosted-tool steps and all part
//!   state: those mean something only to their producer. Readable thinking
//!   is dropped rather than turned into text, which a model would take for
//!   its own answer.
//! - Tool-call ids the target cannot take are rewritten, with their results.
//! - A call nothing answered gets an error result before the conversation
//!   moves on; a result nothing asked for is dropped.
//! - A system message between a call and its results waits until after them.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::protocol::Target;
use crate::spec::{
    Assistant, AssistantPart, Message, Meta, Output, Request, SmolStr, ToolMessage, ToolResult,
};

/// The text an unanswered call is answered with.
pub const NO_RESULT: &str = "No result provided";

/// `request` as `target` takes it; borrowed when the pass would change
/// nothing, as for a conversation with one model, so a long transcript is
/// not copied on every call.
pub fn prepare<'a>(request: &'a Request, target: &Target<'_>) -> Cow<'a, Request> {
    if ready(request, target) {
        Cow::Borrowed(request)
    } else {
        Cow::Owned(rebuild(request.clone(), target))
    }
}

/// Whether `assistant` goes back to the protocol, provider and model that
/// produced it.
fn native(assistant: &Assistant, target: &Target<'_>) -> bool {
    assistant.origin.as_ref().is_some_and(|origin| {
        origin.protocol == target.provider.protocol.key()
            && origin.provider == *target.provider.id
            && origin.model == target.model
    })
}

/// Whether [`rebuild`] would return `request` as it is: every part fits
/// the target and every call is answered right after it.
fn ready(request: &Request, target: &Target<'_>) -> bool {
    let protocol = &target.provider.protocol;
    let mut pending: Vec<&SmolStr> = Vec::new();
    for message in &request.messages {
        match message {
            Message::Tool(tool) => {
                for result in &tool.results {
                    let Some(at) = pending.iter().position(|id| **id == result.id) else {
                        return false;
                    };
                    pending.remove(at);
                }
                if tool.results.is_empty() {
                    return false;
                }
            }
            _ if !pending.is_empty() => return false,
            Message::Assistant(assistant) => {
                let fits = |part: &AssistantPart| match part {
                    AssistantPart::Text(_) | AssistantPart::Media(_) => part.meta().is_empty(),
                    AssistantPart::ToolCall(call) => {
                        !call.provider_executed
                            && call.meta.is_empty()
                            && protocol.tool_id(&call.id).is_none()
                    }
                    _ => false,
                };
                if assistant.content.is_empty()
                    || !native(assistant, target) && !assistant.content.iter().all(fits)
                {
                    return false;
                }
                pending.extend(assistant.tool_calls().map(|call| &call.id));
            }
            Message::System(_) | Message::User(_) => {}
        }
    }
    pending.is_empty()
}

/// The pass itself, on a request of its own.
fn rebuild(mut request: Request, target: &Target<'_>) -> Request {
    let protocol = &target.provider.protocol;
    let native = |assistant: &Assistant| native(assistant, target);
    let mut ids = Ids::default();
    let messages: Vec<Message> = std::mem::take(&mut request.messages)
        .into_iter()
        .filter_map(|message| match message {
            Message::Assistant(mut assistant) => {
                if !native(&assistant) {
                    assistant.content.retain(|part| match part {
                        AssistantPart::Text(_) | AssistantPart::Media(_) => true,
                        AssistantPart::ToolCall(call) => !call.provider_executed,
                        AssistantPart::Reasoning(_)
                        | AssistantPart::ToolResult(_)
                        | AssistantPart::Opaque(_) => false,
                    });
                    for part in &mut assistant.content {
                        part.meta_mut().clear();
                        if let AssistantPart::ToolCall(call) = part {
                            call.id = ids.rename(&call.id, |id| protocol.tool_id(id));
                        }
                    }
                }
                (!assistant.content.is_empty()).then_some(Message::Assistant(assistant))
            }
            Message::Tool(mut tool) => {
                for result in &mut tool.results {
                    if let Some(id) = ids.renamed.get(&result.id) {
                        result.id.clone_from(id);
                    }
                }
                Some(Message::Tool(tool))
            }
            other => Some(other),
        })
        .collect();
    request.messages = repair(messages);
    request
}

#[derive(Default)]
struct Ids {
    renamed: HashMap<SmolStr, SmolStr>,
    taken: HashSet<SmolStr>,
}

impl Ids {
    fn rename(&mut self, id: &SmolStr, fit: impl Fn(&str) -> Option<String>) -> SmolStr {
        if let Some(renamed) = self.renamed.get(id) {
            return renamed.clone();
        }
        let Some(mut fitted) = fit(id) else {
            self.taken.insert(id.clone());
            return id.clone();
        };
        // Two ids that fit to the same one stay distinct: refit with a
        // suffix, and if the fit swallows it, keep the suffix raw.
        let base = fitted.clone();
        let mut n = 1;
        while self.taken.contains(fitted.as_str()) {
            n += 1;
            fitted = fit(&format!("{id}_{n}"))
                .filter(|refit| !self.taken.contains(refit.as_str()))
                .unwrap_or_else(|| format!("{base}_{n}"));
        }
        let fitted = SmolStr::from(fitted);
        self.taken.insert(fitted.clone());
        self.renamed.insert(id.clone(), fitted.clone());
        fitted
    }
}

/// Pairs every call with a result, in place.
fn repair(messages: Vec<Message>) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len());
    // Calls awaiting results, in call order: (id, name).
    let mut pending: Vec<(SmolStr, SmolStr)> = Vec::new();
    let mut held: Vec<Message> = Vec::new();
    for message in messages {
        match message {
            Message::Tool(mut tool) => {
                tool.results.retain(|result| {
                    let waiting = pending.iter().position(|(id, _)| *id == result.id);
                    waiting.map(|at| pending.remove(at)).is_some()
                });
                if !tool.results.is_empty() {
                    out.push(Message::Tool(tool));
                }
                if pending.is_empty() {
                    out.append(&mut held);
                }
            }
            Message::System(_) if !pending.is_empty() => held.push(message),
            other => {
                answer(&mut out, &mut pending);
                out.append(&mut held);
                if let Message::Assistant(assistant) = &other {
                    pending.extend(
                        assistant
                            .tool_calls()
                            .map(|call| (call.id.clone(), call.name.clone())),
                    );
                }
                out.push(other);
            }
        }
    }
    answer(&mut out, &mut pending);
    out.append(&mut held);
    out
}

fn answer(out: &mut Vec<Message>, pending: &mut Vec<(SmolStr, SmolStr)>) {
    if pending.is_empty() {
        return;
    }
    let results = pending.drain(..).map(|(id, name)| ToolResult {
        id,
        name,
        output: Output::Text(NO_RESULT.to_owned()),
        error: true,
        meta: Meta::new(),
    });
    // Results the conversation already began join the synthetic ones.
    if let Some(Message::Tool(tool)) = out.last_mut() {
        tool.results.extend(results);
    } else {
        out.push(Message::Tool(ToolMessage {
            results: results.collect(),
        }));
    }
}

#[cfg(test)]
mod tests;
