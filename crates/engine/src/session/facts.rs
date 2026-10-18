//! What a session's log says, read back: the conversation the model sees,
//! and the agent's state.

use std::collections::{HashMap, VecDeque};

use agent::{Action, Agent, Input, Toolset};
use llm::{Message, ToolMessage, ToolResult};
use protocol::{Body, Decision, Entry, Outcome};

/// The conversation before `before`, derived from the log's facts. A model
/// reply is the assistant message it stored, provider state included; a
/// reply logged as bare text (by an earlier version) reads back as text.
/// A tool call the model made is answered under the model's id for it.
pub fn transcript(entries: &[Entry], before: u64, tools: &Toolset) -> Vec<Message> {
    let mut messages = Vec::new();
    // Model calls by our seq: the model's id and the tool.
    let mut calls = HashMap::new();
    for e in entries.iter().take_while(|e| e.seq < before) {
        match &e.body {
            Body::User { text }
            | Body::Hook {
                decision: Decision::OnStop { text },
                ..
            } => messages.push(Message::user(text.clone())),
            Body::Assistant { text } => messages.push(Message::assistant(text.clone())),
            Body::ModelResult {
                outcome: Outcome::Done { output },
                ..
            } => match llm::Response::from_value(output) {
                Ok(reply) => messages.push(Message::Assistant(reply.message)),
                Err(_) => {
                    if let Some(text) = output["text"].as_str() {
                        messages.push(Message::assistant(text));
                    }
                }
            },
            Body::ToolCall {
                tool, id: Some(id), ..
            } => {
                calls.insert(e.seq, (id.as_str(), tool.as_str()));
            }
            Body::ToolResult { call, outcome } => {
                let Some(&(id, tool)) = calls.get(call) else {
                    continue;
                };
                let (output, error) = tools.to_model(tool, outcome);
                let result = ToolResult {
                    id: id.into(),
                    name: tool.into(),
                    output,
                    error,
                    meta: llm::Meta::new(),
                };
                match messages.last_mut() {
                    Some(Message::Tool(answers)) => answers.results.push(result),
                    _ => messages.push(Message::Tool(ToolMessage {
                        results: vec![result],
                    })),
                }
            }
            _ => {}
        }
    }
    messages
}

/// Steps a fresh `agent` through every input the log records, so it is in
/// the state it was in when the log was written. Returns what it asked for
/// that the log does not show: the daemon stopped between an outcome and the
/// intents it led to.
pub fn replay(agent: &mut dyn Agent, entries: &[Entry]) -> VecDeque<Action> {
    let mut asked = VecDeque::new();
    for e in entries {
        let input = match &e.body {
            Body::User { text }
            | Body::Hook {
                decision: Decision::OnStop { text },
                ..
            } => Input::User { text: text.clone() },
            Body::ToolResult { call, outcome } => Input::ToolDone {
                call: *call,
                outcome: outcome.clone(),
            },
            Body::ModelResult { call, outcome } => Input::ModelDone {
                call: *call,
                outcome: outcome.clone(),
            },
            Body::Assistant { .. } | Body::ToolCall { .. } | Body::ModelCall { .. } => {
                // A log that disagrees with the agent (an earlier version
                // wrote it) leaves nothing to redo.
                if asked.front().is_some_and(|action| commits(action, &e.body)) {
                    asked.pop_front();
                } else {
                    asked.clear();
                }
                continue;
            }
            Body::ToolProcess { .. } | Body::Hook { .. } => continue,
        };
        asked.extend(agent.step(input));
    }
    asked
}

/// Whether `body` is the entry carrying out `action`.
fn commits(action: &Action, body: &Body) -> bool {
    match (action, body) {
        (Action::Emit(emitted), body) => emitted == body,
        (Action::RunTool { tool, .. }, Body::ToolCall { tool: called, .. }) => tool == called,
        (Action::CallModel, Body::ModelCall { .. }) => true,
        _ => false,
    }
}
