//! What a session's log says, read back: the conversation the model sees,
//! the agent's state, what waits for the user or the agent, and a recap.

use std::collections::{HashMap, VecDeque};

use agent::{Action, Agent, Input, Toolset};
use llm::{Message, ToolMessage, ToolResult};
use protocol::ledger::Ledger;
use protocol::{Body, DaemonMsg, Decision, Entry, Outcome, Recap};

/// The conversation before `before`, derived from the log's facts. A model
/// reply is the assistant message it stored, provider state included; a
/// reply logged as bare text (by an earlier version) reads back as text.
/// A tool call the model made is answered under the model's id for it. The
/// session's context comes first, wherever it was logged.
pub fn transcript(entries: &[Entry], before: u64, tools: &Toolset) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut context = 0;
    // Model calls by our seq: the model's id and the tool.
    let mut calls = HashMap::new();
    for e in entries.iter().take_while(|e| e.seq < before) {
        match &e.body {
            Body::User { text }
            | Body::Steer { text, .. }
            | Body::Hook {
                decision: Decision::OnStop { text },
                ..
            } => messages.push(Message::user(text.clone())),
            Body::Assistant { text } => messages.push(Message::assistant(text.clone())),
            Body::Context { text } => {
                messages.insert(context, Message::user(text.clone()));
                context += 1;
            }
            Body::Fired { outcome, .. } => {
                messages.push(Message::user(agent::child_result(outcome)));
            }
            Body::ModelResult {
                outcome: Outcome::Done { output },
                ..
            } => {
                let output = output.value();
                match llm::Response::from_value(&output) {
                    Ok(reply) => messages.push(Message::Assistant(reply.message)),
                    Err(_) => {
                        if let Some(text) = output["text"].as_str() {
                            messages.push(Message::assistant(text));
                        }
                    }
                }
            }
            Body::ToolCall {
                tool, id: Some(id), ..
            } => {
                calls.insert(e.seq, (id.as_str(), tool.as_str()));
            }
            Body::ToolResult { call, outcome, .. } => {
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
            Body::ToolResult { call, outcome, .. } => Input::ToolDone {
                call: *call,
                outcome: outcome.clone(),
            },
            Body::ModelResult { call, outcome } => Input::ModelDone {
                call: *call,
                outcome: outcome.clone(),
            },
            Body::Fired { watch, outcome } => Input::ChildDone {
                watch: *watch,
                outcome: outcome.clone(),
            },
            Body::Assistant { .. }
            | Body::ToolCall { .. }
            | Body::ModelCall { .. }
            | Body::Link { .. } => {
                // A log that disagrees with the agent (an earlier version
                // wrote it) leaves nothing to redo.
                if asked.front().is_some_and(|action| commits(action, &e.body)) {
                    asked.pop_front();
                } else {
                    asked.clear();
                }
                continue;
            }
            // The turn was cut short: what it asked for is not done.
            Body::Interrupt => {
                asked.clear();
                continue;
            }
            // Read by the model, not stepped through the agent.
            Body::Steer { .. }
            | Body::Queued { .. }
            | Body::Progress { .. }
            | Body::Settings(_)
            | Body::ToolProcess { .. }
            | Body::Hook { .. }
            | Body::Ask { .. }
            | Body::Watch { .. }
            | Body::Environment(_)
            | Body::Extensions { .. }
            | Body::Ext { .. }
            | Body::Context { .. }
            | Body::Command { .. }
            | Body::CommandResult { .. } => continue,
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
        (Action::Spawn { input }, Body::Link { input: linked, .. }) => input == linked,
        _ => false,
    }
}

/// What a client is sent for the ask about tool call `call`, while it
/// waits.
pub fn ask(ledger: &Ledger, call: u64) -> Option<DaemonMsg> {
    let (tool, input) = ledger.asked(call)?;
    Some(DaemonMsg::Ask {
        call,
        tool: tool.to_owned(),
        input: input.clone(),
    })
}

/// What a client attaching after `since` is told: the user's last message
/// and what the agent did since, `reply` its last answer. `None` when
/// nothing worth telling is new, such as only an environment.
pub fn recap(ledger: &Ledger, reply: Option<&str>, since: u64) -> Option<Recap> {
    if ledger.seq() <= since {
        return None;
    }
    let recap = Recap {
        prompt: ledger.prompt().map(short),
        tools: ledger.turn_tools().to_vec(),
        failed: ledger.turn_failed(),
        running: ledger.turn_running(),
        reply: reply.map(short),
        asks: u32::try_from(ledger.inbox().len()).unwrap_or(u32::MAX),
    };
    (recap != Recap::default()).then_some(recap)
}

/// The answer of the last reply among `entries`, as a recap tells it: an
/// assistant message, or a model reply with text.
pub fn reply(entries: &[Entry]) -> Option<String> {
    let mut reply = None;
    for e in entries {
        match &e.body {
            Body::Assistant { text } => reply = Some(text.clone()),
            Body::ModelResult {
                outcome: Outcome::Done { output },
                ..
            } => {
                let text = llm::Response::from_value(&output.value()).map(|r| r.message.text());
                if let Ok(text) = text
                    && !text.is_empty()
                {
                    reply = Some(text);
                }
            }
            _ => {}
        }
    }
    reply
}

/// The first line of `text`, cut to a recap's width.
pub(super) fn short(text: &str) -> String {
    const WIDTH: usize = 60;
    let text = text.trim();
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(WIDTH) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None if line.len() < text.len() => format!("{line}…"),
        None => line.to_owned(),
    }
}
