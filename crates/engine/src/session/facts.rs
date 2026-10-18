//! What a session's log says, read back: the conversation the model sees,
//! the agent's state, what waits for the user, and a recap.

use std::collections::{HashMap, HashSet, VecDeque};

use agent::{Action, Agent, Input, Toolset};
use llm::{Message, ToolMessage, ToolResult};
use protocol::{Body, DaemonMsg, Decision, Entry, Outcome, Recap};

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
            Body::ToolProcess { .. } | Body::Hook { .. } | Body::Ask { .. } => continue,
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

/// The session's inbox: the tool calls asked and not yet answered, in the
/// order they were asked.
pub fn inbox(entries: &[Entry]) -> Vec<u64> {
    let answered: HashSet<u64> = entries
        .iter()
        .filter_map(|e| match e.body {
            Body::Hook {
                decision:
                    Decision::Approval {
                        call: Some(call), ..
                    },
                ..
            } => Some(call),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .filter_map(|e| match e.body {
            Body::Ask { call } if !answered.contains(&call) => Some(call),
            _ => None,
        })
        .collect()
}

/// What a client is sent for the ask about tool call `call`.
pub fn ask(entries: &[Entry], call: u64) -> Option<DaemonMsg> {
    let at = entries.binary_search_by_key(&call, |e| e.seq).ok()?;
    match &entries[at].body {
        Body::ToolCall { tool, input, .. } => Some(DaemonMsg::Ask {
            call,
            tool: tool.clone(),
            input: input.clone(),
        }),
        _ => None,
    }
}

/// What a client attaching after `since` is told: the user's last message
/// and what the agent did since. `None` when nothing is new.
pub fn recap(entries: &[Entry], since: u64) -> Option<Recap> {
    if entries.last().is_none_or(|e| e.seq <= since) {
        return None;
    }
    let asks = inbox(entries);
    let start = entries
        .iter()
        .rposition(|e| matches!(e.body, Body::User { .. }));
    let mut recap = Recap {
        prompt: start.and_then(|at| match &entries[at].body {
            Body::User { text } => Some(short(text)),
            _ => None,
        }),
        asks: u32::try_from(asks.len()).unwrap_or(u32::MAX),
        ..Recap::default()
    };
    let after = &entries[start.map_or(0, |at| at + 1)..];
    let finished: HashSet<u64> = after
        .iter()
        .filter_map(|e| match e.body {
            Body::ToolResult { call, .. } | Body::ModelResult { call, .. } => Some(call),
            _ => None,
        })
        .collect();
    for e in after {
        let open = !finished.contains(&e.seq) && !asks.contains(&e.seq);
        match &e.body {
            Body::ToolCall { tool, .. } => {
                recap.tools.push(tool.clone());
                recap.running += u32::from(open);
            }
            Body::ModelCall { .. } => recap.running += u32::from(open),
            Body::ToolResult { outcome, .. } | Body::ModelResult { outcome, .. }
                if !matches!(outcome, Outcome::Done { .. }) =>
            {
                recap.failed += 1;
            }
            Body::Assistant { text } => recap.reply = Some(short(text)),
            Body::ModelResult {
                outcome: Outcome::Done { output },
                ..
            } => {
                let text = llm::Response::from_value(output).map(|r| r.message.text());
                if let Ok(text) = text.as_deref()
                    && !text.is_empty()
                {
                    recap.reply = Some(short(text));
                }
            }
            _ => {}
        }
    }
    Some(recap)
}

/// The first line of `text`, cut to a recap's width.
fn short(text: &str) -> String {
    const WIDTH: usize = 60;
    let text = text.trim();
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(WIDTH) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None if line.len() < text.len() => format!("{line}…"),
        None => line.to_owned(),
    }
}
