//! The scans of the whole log the ledger replaced, kept as its oracle: a
//! ledger folded from a log says what each of these says of it.

use std::collections::{HashMap, HashSet, VecDeque};

use protocol::ledger::{Call, Ledger, OutcomeKind, Phase};
use protocol::{
    Body, Decision, Entry, Environment, Installed, Outcome, Overrides, Recap, SessionId, WaitOn,
};

use ext::host::Toolset;
use llm::{Message, ToolMessage, ToolResult};

use super::facts;

mod tests;

/// Asserts that `ledger` says of `entries` what the scans say.
pub fn check(entries: &[Entry], ledger: &Ledger) {
    assert_eq!(ledger.seq(), entries.last().map_or(0, |e| e.seq));
    assert_eq!(ledger.inbox(), inbox(entries), "inbox");
    for &call in ledger.inbox() {
        assert_eq!(
            facts::ask(ledger, call).map(|m| format!("{m:?}")),
            ask(entries, call),
            "ask {call}"
        );
    }
    let dangling: Vec<_> = ledger
        .open_calls()
        .map(|(seq, open)| {
            let asked = match open.phase {
                Phase::Waiting(WaitOn::Approval) => Asked::Waiting,
                Phase::Settled(OutcomeKind::Failed) => Asked::Denied,
                _ => Asked::No,
            };
            let call = match &open.call {
                Call::Model { .. } => None,
                Call::Tool { tool, input, .. } => Some((tool.clone(), input.raw().to_owned())),
            };
            (
                seq,
                call,
                open.processes.clone(),
                asked,
                open.progress.clone(),
            )
        })
        .collect();
    assert_eq!(dangling, dangling_calls(entries), "dangling calls");
    let untaken: VecDeque<_> = ledger
        .untaken()
        .iter()
        .map(|q| (q.seq, q.text.clone(), q.steer))
        .collect();
    assert_eq!(untaken, self::untaken(entries), "untaken");
    let commands: Vec<u64> = ledger.open_commands().collect();
    assert_eq!(commands, dangling_commands(entries), "commands");
    assert_eq!(
        ledger.background(),
        background_groups(entries),
        "background"
    );
    let armed: Vec<_> = ledger.armed().collect();
    assert_eq!(armed, self::armed(entries).collect::<Vec<_>>(), "armed");
    let links: Vec<_> = ledger
        .links()
        .iter()
        .map(|link| {
            let state = if !ledger.watched(link.child) {
                Linked::Unwatched
            } else if ledger.armed().any(|(_, c)| c == link.child) {
                Linked::Armed
            } else {
                Linked::Fired
            };
            (link.seq, link.child, link.input.clone(), state)
        })
        .collect();
    assert_eq!(links, relink(entries), "links");
    assert_eq!(ledger.result(), result(entries).as_ref(), "result");
    assert_eq!(ledger.settings(), overrides(entries), "settings");
    assert_eq!(ledger.env(), logged_env(entries), "environment");
    assert_eq!(ledger.extensions(), extensions(entries), "extensions");
    assert_eq!(
        ledger.has_context(),
        entries
            .iter()
            .any(|e| matches!(e.body, Body::Context { .. })),
        "context"
    );
    let hooks: Vec<Body> = ledger
        .trailing_hooks()
        .iter()
        .map(|(hook, decision)| Body::Hook {
            hook: hook.clone(),
            decision: decision.clone(),
        })
        .collect();
    assert_eq!(hooks, trailing_hooks(entries), "trailing hooks");
    for since in [0, ledger.seq().saturating_sub(1), ledger.seq()] {
        let after = entries.partition_point(|e| e.seq <= ledger.turn_start());
        let reply = facts::reply(&entries[after..]);
        assert_eq!(
            facts::recap(ledger, reply.as_deref(), since),
            recap(entries, since),
            "recap since {since}"
        );
    }
}

/// Folds `entries` one at a time, checking the ledger after each.
pub fn check_each(entries: &[Entry]) {
    let mut ledger = Ledger::default();
    for (at, e) in entries.iter().enumerate() {
        ledger.apply(e);
        check(&entries[..=at], &ledger);
    }
}

pub fn untaken(entries: &[Entry]) -> VecDeque<(u64, String, bool)> {
    let mut waiting = VecDeque::new();
    for e in entries {
        match &e.body {
            Body::Queued { text, steer } => waiting.push_back((e.seq, text.clone(), *steer)),
            Body::User { .. } | Body::Command { .. } => drop(waiting.pop_front()),
            Body::Steer { queued, .. } => waiting.retain(|(seq, ..)| seq != queued),
            _ => {}
        }
    }
    waiting
}

fn finished(entries: &[Entry]) -> HashSet<u64> {
    entries
        .iter()
        .filter_map(|e| match e.body {
            Body::ToolResult { call, .. } | Body::ModelResult { call, .. } => Some(call),
            _ => None,
        })
        .collect()
}

fn partial(entries: &[Entry], call: u64) -> Option<String> {
    let mut output: Option<String> = None;
    for e in entries.iter().filter(|e| e.seq > call) {
        if let Body::Progress { call: of, text } = &e.body
            && *of == call
        {
            output.get_or_insert_default().push_str(text);
        }
    }
    output
}

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

fn ask(entries: &[Entry], call: u64) -> Option<String> {
    let at = entries.binary_search_by_key(&call, |e| e.seq).ok()?;
    match &entries[at].body {
        Body::ToolCall { tool, input, .. } => Some(format!(
            "{:?}",
            protocol::DaemonMsg::Ask {
                call,
                tool: tool.clone(),
                input: input.clone(),
            }
        )),
        _ => None,
    }
}

fn recap(entries: &[Entry], since: u64) -> Option<Recap> {
    if entries.last().is_none_or(|e| e.seq <= since) {
        return None;
    }
    let asks = inbox(entries);
    let start = entries
        .iter()
        .rposition(|e| matches!(e.body, Body::User { .. }));
    let mut recap = Recap {
        prompt: start.and_then(|at| match &entries[at].body {
            Body::User { text } => Some(facts::short(text)),
            _ => None,
        }),
        asks: u32::try_from(asks.len()).unwrap_or(u32::MAX),
        ..Recap::default()
    };
    let after = &entries[start.map_or(0, |at| at + 1)..];
    let finished = finished(after);
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
            Body::Assistant { text } => recap.reply = Some(facts::short(text)),
            Body::ModelResult {
                outcome: Outcome::Done { output },
                ..
            } => {
                let text = llm::Response::from_value(&output.value()).map(|r| r.message.text());
                if let Ok(text) = text.as_deref()
                    && !text.is_empty()
                {
                    recap.reply = Some(facts::short(text));
                }
            }
            _ => {}
        }
    }
    (recap != Recap::default()).then_some(recap)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    /// Not asked, or approved.
    No,
    Waiting,
    Denied,
}

type Dangling = (
    u64,
    Option<(String, String)>,
    Vec<(u32, String)>,
    Asked,
    Option<String>,
);

fn dangling_calls(entries: &[Entry]) -> Vec<Dangling> {
    let finished = finished(entries);
    let mut dangling: Vec<Dangling> = Vec::new();
    let find = |dangling: &mut Vec<Dangling>, call: u64| -> Option<usize> {
        dangling.iter().rposition(|d| d.0 == call)
    };
    for e in entries {
        let call = match &e.body {
            Body::ToolProcess {
                call, pgid, boot, ..
            } => {
                if let Some(at) = find(&mut dangling, *call) {
                    dangling[at].2.push((*pgid, boot.clone()));
                }
                continue;
            }
            Body::Ask { call } => {
                if let Some(at) = find(&mut dangling, *call) {
                    dangling[at].3 = Asked::Waiting;
                }
                continue;
            }
            Body::Hook {
                decision:
                    Decision::Approval {
                        call: Some(call),
                        approved,
                        ..
                    },
                ..
            } => {
                if let Some(at) = find(&mut dangling, *call) {
                    dangling[at].3 = if *approved { Asked::No } else { Asked::Denied };
                }
                continue;
            }
            _ if finished.contains(&e.seq) => continue,
            Body::ToolCall { tool, input, .. } => Some((tool.clone(), input.raw().to_owned())),
            Body::ModelCall { .. } => None,
            _ => continue,
        };
        dangling.push((e.seq, call, Vec::new(), Asked::No, None));
    }
    for d in &mut dangling {
        d.4 = partial(entries, d.0);
    }
    dangling
}

fn dangling_commands(entries: &[Entry]) -> Vec<u64> {
    let done: HashSet<u64> = entries
        .iter()
        .filter_map(|e| match e.body {
            Body::CommandResult { call, .. } => Some(call),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .filter(|e| matches!(e.body, Body::Command { .. }) && !done.contains(&e.seq))
        .map(|e| e.seq)
        .collect()
}

fn background_groups(entries: &[Entry]) -> Vec<(u32, String)> {
    entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolProcess {
                pgid,
                boot,
                background: true,
                ..
            } => Some((*pgid, boot.clone())),
            _ => None,
        })
        .collect()
}

pub fn armed(entries: &[Entry]) -> impl Iterator<Item = (u64, SessionId)> {
    let fired: HashSet<u64> = entries
        .iter()
        .filter_map(|e| match e.body {
            Body::Fired { watch, .. } => Some(watch),
            _ => None,
        })
        .collect();
    entries.iter().filter_map(move |e| match e.body {
        Body::Watch { child } if !fired.contains(&e.seq) => Some((e.seq, child)),
        _ => None,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Linked {
    Unwatched,
    Armed,
    Fired,
}

fn relink(entries: &[Entry]) -> Vec<(u64, SessionId, String, Linked)> {
    let armed: HashSet<SessionId> = armed(entries).map(|(_, child)| child).collect();
    let watched: HashSet<SessionId> = entries
        .iter()
        .filter_map(|e| match e.body {
            Body::Watch { child } => Some(child),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::Link { child, input } => {
                let linked = if !watched.contains(child) {
                    Linked::Unwatched
                } else if armed.contains(child) {
                    Linked::Armed
                } else {
                    Linked::Fired
                };
                Some((e.seq, *child, input.clone(), linked))
            }
            _ => None,
        })
        .collect()
}

fn result(entries: &[Entry]) -> Option<Outcome> {
    entries.iter().rev().find_map(|e| match &e.body {
        Body::Assistant { text } => Some(Outcome::Done {
            output: text.as_str().into(),
        }),
        Body::ToolResult { outcome, .. }
        | Body::ModelResult { outcome, .. }
        | Body::Fired { outcome, .. } => Some(outcome.clone()),
        Body::Interrupt { .. } => Some(Outcome::Interrupted { output: None }),
        _ => None,
    })
}

fn overrides(entries: &[Entry]) -> (Overrides, Option<u64>) {
    entries
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            Body::Settings(set) => Some((set.clone(), Some(e.seq))),
            _ => None,
        })
        .unwrap_or_default()
}

fn logged_env(entries: &[Entry]) -> Option<&Environment> {
    entries.iter().rev().find_map(|e| match &e.body {
        Body::Environment(env) => Some(env),
        _ => None,
    })
}

fn extensions(entries: &[Entry]) -> Option<&[Installed]> {
    entries.iter().rev().find_map(|e| match &e.body {
        Body::Extensions { installed } => Some(installed.as_slice()),
        _ => None,
    })
}

fn trailing_hooks(entries: &[Entry]) -> Vec<Body> {
    let mut logged: Vec<Body> = entries
        .iter()
        .rev()
        .map(|e| &e.body)
        .filter(|b| !matches!(b, Body::Extensions { .. } | Body::Environment(_)))
        .take_while(|b| matches!(b, Body::Hook { .. }))
        .cloned()
        .collect();
    logged.reverse();
    logged
}

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
                messages.push(Message::user(ext::child_result(outcome)));
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
