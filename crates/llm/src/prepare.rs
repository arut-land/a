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

use std::collections::{HashMap, HashSet};

use crate::protocol::Target;
use crate::spec::{AssistantPart, Message, Origin, Output, Request, ToolMessage, ToolResult};

/// The text an unanswered call is answered with.
pub const NO_RESULT: &str = "No result provided";

pub fn prepare(request: &Request, target: &Target<'_>) -> Request {
    let key = target.provider.protocol.key();
    let here = Origin {
        protocol: key.to_owned(),
        provider: target.provider.id.to_string(),
        model: target.model.to_owned(),
    };
    let mut ids = Ids::default();
    let mut messages = Vec::with_capacity(request.messages.len());
    for message in &request.messages {
        match message {
            Message::Assistant(assistant) => {
                let native = assistant.origin.as_ref() == Some(&here);
                let mut assistant = assistant.clone();
                if !native {
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
                            call.id =
                                ids.rename(&call.id, |id| target.provider.protocol.tool_id(id));
                        }
                    }
                }
                if !assistant.content.is_empty() {
                    messages.push(Message::Assistant(assistant));
                }
            }
            Message::Tool(tool) => {
                let mut tool = tool.clone();
                for result in &mut tool.results {
                    if let Some(id) = ids.renamed.get(&result.id) {
                        result.id.clone_from(id);
                    }
                }
                messages.push(Message::Tool(tool));
            }
            other => messages.push(other.clone()),
        }
    }
    Request {
        messages: repair(messages),
        ..request.clone()
    }
}

#[derive(Default)]
struct Ids {
    renamed: HashMap<String, String>,
    taken: HashSet<String>,
}

impl Ids {
    fn rename(&mut self, id: &str, fit: impl Fn(&str) -> Option<String>) -> String {
        if let Some(renamed) = self.renamed.get(id) {
            return renamed.clone();
        }
        let Some(mut fitted) = fit(id) else {
            self.taken.insert(id.to_owned());
            return id.to_owned();
        };
        // Two ids that fit to the same one stay distinct: refit with a
        // suffix, and if the fit swallows it, keep the suffix raw.
        let base = fitted.clone();
        let mut n = 1;
        while self.taken.contains(&fitted) {
            n += 1;
            let suffixed = format!("{id}_{n}");
            fitted = fit(&suffixed)
                .filter(|refit| !self.taken.contains(refit))
                .unwrap_or_else(|| format!("{base}_{n}"));
        }
        self.taken.insert(fitted.clone());
        self.renamed.insert(id.to_owned(), fitted.clone());
        fitted
    }
}

/// Pairs every call with a result, in place.
fn repair(messages: Vec<Message>) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len());
    // Calls awaiting results, in call order: (id, name).
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut held: Vec<Message> = Vec::new();
    for message in messages {
        match message {
            Message::Tool(tool) => {
                let results: Vec<ToolResult> = tool
                    .results
                    .into_iter()
                    .filter(|result| {
                        let waiting = pending.iter().position(|(id, _)| *id == result.id);
                        waiting.map(|at| pending.remove(at)).is_some()
                    })
                    .collect();
                if !results.is_empty() {
                    out.push(Message::Tool(ToolMessage { results }));
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

fn answer(out: &mut Vec<Message>, pending: &mut Vec<(String, String)>) {
    if pending.is_empty() {
        return;
    }
    let results = pending
        .drain(..)
        .map(|(id, name)| ToolResult {
            id,
            name,
            output: Output::Text(NO_RESULT.to_owned()),
            error: true,
            meta: crate::Meta::new(),
        })
        .collect();
    // Results the conversation already began join the synthetic ones.
    if let Some(Message::Tool(tool)) = out.last_mut() {
        tool.results.extend::<Vec<_>>(results);
    } else {
        out.push(Message::Tool(ToolMessage { results }));
    }
}

#[cfg(test)]
mod tests;
