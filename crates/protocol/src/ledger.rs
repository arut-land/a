//! What a session's log says now, folded once per entry as it commits or
//! loads, so that no question about the log scans it: which calls are open
//! and where each stands, what waits for the user and for the agent, which
//! watches are armed, and the settings, environment and context last
//! logged. Pure: the daemon and any client fold the same entries to the
//! same ledger.

use std::collections::VecDeque;

use crate::{Body, Decision, Entry, Environment, Installed, Json, Outcome, Overrides, SessionId};

#[derive(Clone, Debug, Default)]
pub struct Ledger {
    /// The seq of the last entry folded.
    seq: u64,
    /// Model and tool calls with an intent and no outcome, by seq.
    open: BySeq<Open>,
    /// The open tool calls asked about and not yet answered, in the order
    /// they were asked: the session's inbox.
    inbox: Vec<u64>,
    /// Input queued and not yet taken up, in order.
    untaken: VecDeque<Queued>,
    /// Slash commands with an intent and no outcome.
    commands: BySeq<()>,
    /// Watches that have not fired, by seq.
    armed: BySeq<SessionId>,
    /// Every child linked, in order, and the children ever watched.
    links: Vec<Link>,
    watched: Vec<SessionId>,
    /// Background process groups started, with the boot they ran in.
    background: Vec<(u32, String)>,
    settings: Option<(u64, Overrides)>,
    env: Option<Environment>,
    extensions: Option<Vec<Installed>>,
    context: bool,
    /// The hook decisions ending the log, but for environments and
    /// extensions logged between them.
    hooks: Vec<(String, Decision)>,
    /// What the session last settled with, as a child hands it back.
    result: Option<Outcome>,
    turn: Turn,
}

/// A call between its intent and its outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    pub call: Call,
    pub phase: Phase,
    /// The process groups it started, with the boot each ran in.
    pub processes: Vec<(u32, String)>,
    /// The output it committed as it ran.
    pub progress: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Model,
    Tool { tool: String, input: Json },
}

/// Where an open call stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Its intent is committed; it runs, or is about to.
    Called,
    /// It waits for the user's approval: it is in the inbox.
    Asked,
    /// The user answered.
    Answered { approved: bool },
}

/// Input taken in and not yet taken up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Queued {
    pub seq: u64,
    pub text: String,
    pub steer: bool,
}

/// A child session linked, and the input it starts with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub seq: u64,
    pub child: SessionId,
    pub input: String,
}

/// What the current turn did, from its user message on: what a recap
/// tells but the reply, which only a model's reply format can read.
#[derive(Clone, Debug, Default)]
struct Turn {
    /// The seq of its user message; 0 before the first.
    start: u64,
    prompt: Option<String>,
    tools: Vec<String>,
    failed: u32,
}

impl Ledger {
    /// Folds every entry of `entries`, in order.
    pub fn of<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Self {
        let mut ledger = Self::default();
        for e in entries {
            ledger.apply(e);
        }
        ledger
    }

    /// Folds entry `e`, the one after the last folded. An entry that does
    /// not fit what came before (an outcome for no open call, an answer for
    /// no ask) changes nothing it cannot apply to, as when an earlier
    /// version wrote it.
    pub fn apply(&mut self, e: &Entry) {
        self.seq = e.seq;
        let seq = e.seq;
        match &e.body {
            Body::Hook { .. } | Body::Extensions { .. } | Body::Environment(_) => {}
            _ => self.hooks.clear(),
        }
        match &e.body {
            Body::User { text } => {
                self.taken();
                self.turn = Turn {
                    start: seq,
                    prompt: Some(text.clone()),
                    ..Turn::default()
                };
            }
            Body::Queued { text, steer } => self.untaken.push_back(Queued {
                seq,
                text: text.clone(),
                steer: *steer,
            }),
            Body::Steer { queued, .. } => self.untaken.retain(|q| q.seq != *queued),
            Body::Command { .. } => {
                self.taken();
                self.commands.insert(seq, ());
            }
            Body::CommandResult { call, .. } => {
                self.commands.remove(*call);
            }
            Body::ToolCall { .. }
            | Body::ModelCall { .. }
            | Body::ToolProcess { .. }
            | Body::Progress { .. }
            | Body::ToolResult { .. }
            | Body::ModelResult { .. }
            | Body::Ask { .. }
            | Body::Hook { .. } => self.track(seq, &e.body),
            Body::Link { child, input } => self.links.push(Link {
                seq,
                child: *child,
                input: input.clone(),
            }),
            Body::Watch { child } => {
                self.armed.insert(seq, *child);
                if !self.watched.contains(child) {
                    self.watched.push(*child);
                }
            }
            Body::Fired { watch, outcome } => {
                self.armed.remove(*watch);
                self.result = Some(outcome.clone());
            }
            Body::Assistant { text } => {
                self.result = Some(Outcome::Done {
                    output: text.as_str().into(),
                });
            }
            Body::Interrupt => self.result = Some(Outcome::Interrupted { output: None }),
            Body::Environment(env) => self.env = Some(env.clone()),
            Body::Extensions { installed } => self.extensions = Some(installed.clone()),
            Body::Context { .. } => self.context = true,
            Body::Settings(set) => self.settings = Some((seq, set.clone())),
            Body::Ext { .. } => {}
        }
    }

    /// Folds an entry about a call: its intent, processes, progress,
    /// outcome, ask or a hook's decision on it.
    fn track(&mut self, seq: u64, body: &Body) {
        match body {
            Body::ToolCall { tool, input, .. } => {
                self.turn.tools.push(tool.clone());
                self.open(
                    seq,
                    Call::Tool {
                        tool: tool.clone(),
                        input: input.clone(),
                    },
                );
            }
            Body::ModelCall { .. } => self.open(seq, Call::Model),
            Body::ToolProcess {
                call,
                pgid,
                boot,
                background,
            } => {
                if let Some(open) = self.open.get_mut(*call) {
                    open.processes.push((*pgid, boot.clone()));
                }
                if *background {
                    self.background.push((*pgid, boot.clone()));
                }
            }
            Body::Progress { call, text } => {
                if let Some(open) = self.open.get_mut(*call) {
                    open.progress.get_or_insert_default().push_str(text);
                }
            }
            Body::ToolResult { call, outcome, .. } | Body::ModelResult { call, outcome } => {
                if !matches!(outcome, Outcome::Done { .. }) {
                    self.turn.failed += 1;
                }
                self.result = Some(outcome.clone());
                if self.open.remove(*call).is_some() {
                    self.inbox.retain(|c| c != call);
                }
            }
            // The first answer wins: an answered call is not asked again.
            Body::Ask { call } => {
                if let Some(open) = self.open.get_mut(*call)
                    && matches!(open.call, Call::Tool { .. })
                    && open.phase == Phase::Called
                {
                    open.phase = Phase::Asked;
                    self.inbox.push(*call);
                }
            }
            Body::Hook { hook, decision } => {
                if let Decision::Approval {
                    call: Some(call),
                    approved,
                    ..
                } = decision
                    && let Some(open) = self.open.get_mut(*call)
                {
                    open.phase = Phase::Answered {
                        approved: *approved,
                    };
                    self.inbox.retain(|c| c != call);
                }
                self.hooks.push((hook.clone(), decision.clone()));
            }
            _ => {}
        }
    }

    fn open(&mut self, seq: u64, call: Call) {
        self.open.insert(
            seq,
            Open {
                call,
                phase: Phase::Called,
                processes: Vec::new(),
                progress: None,
            },
        );
    }

    /// A `user` or `command` entry takes up the oldest input waiting.
    fn taken(&mut self) {
        self.untaken.pop_front();
    }

    /// The seq of the last entry folded: how many the log has.
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// The open calls, by seq.
    pub fn open_calls(&self) -> impl Iterator<Item = (u64, &Open)> {
        self.open.iter()
    }

    pub fn call(&self, seq: u64) -> Option<&Open> {
        self.open.get(seq)
    }

    /// The tool calls waiting for the user's approval, in the order asked.
    pub fn inbox(&self) -> &[u64] {
        &self.inbox
    }

    /// The tool and input of call `call`, while it waits in the inbox.
    pub fn asked(&self, call: u64) -> Option<(&str, &Json)> {
        match self.open.get(call) {
            Some(Open {
                call: Call::Tool { tool, input },
                phase: Phase::Asked,
                ..
            }) => Some((tool, input)),
            _ => None,
        }
    }

    pub fn untaken(&self) -> &VecDeque<Queued> {
        &self.untaken
    }

    /// Slash commands with an intent and no outcome, by seq.
    pub fn open_commands(&self) -> impl Iterator<Item = u64> + '_ {
        self.commands.iter().map(|(seq, ())| seq)
    }

    /// The watches that have not fired, as `(seq, child)`, by seq.
    pub fn armed(&self) -> impl Iterator<Item = (u64, SessionId)> + '_ {
        self.armed.iter().map(|(seq, &child)| (seq, child))
    }

    pub fn links(&self) -> &[Link] {
        &self.links
    }

    /// Whether `child` was ever watched.
    pub fn watched(&self, child: SessionId) -> bool {
        self.watched.contains(&child)
    }

    /// The background process groups started, with the boot each ran in.
    pub fn background(&self) -> &[(u32, String)] {
        &self.background
    }

    /// The session's own settings, and the seq of the entry that set them.
    pub fn settings(&self) -> (Overrides, Option<u64>) {
        self.settings
            .clone()
            .map_or_else(Default::default, |(seq, set)| (set, Some(seq)))
    }

    /// The environment most recently logged.
    pub const fn env(&self) -> Option<&Environment> {
        self.env.as_ref()
    }

    /// The extensions most recently logged, if any were.
    pub fn extensions(&self) -> Option<&[Installed]> {
        self.extensions.as_deref()
    }

    /// Whether the session's context is logged.
    pub const fn has_context(&self) -> bool {
        self.context
    }

    /// The hook decisions ending the log, environments and extensions
    /// logged between them aside.
    pub fn trailing_hooks(&self) -> &[(String, Decision)] {
        &self.hooks
    }

    /// What the session last settled with: its last answer or outcome.
    pub const fn result(&self) -> Option<&Outcome> {
        self.result.as_ref()
    }

    /// The seq of the current turn's user message; 0 before the first.
    pub const fn turn_start(&self) -> u64 {
        self.turn.start
    }

    /// The current turn's user message.
    pub fn prompt(&self) -> Option<&str> {
        self.turn.prompt.as_deref()
    }

    /// The tools the current turn called, in order.
    pub fn turn_tools(&self) -> &[String] {
        &self.turn.tools
    }

    /// How many of the current turn's calls did not finish well.
    pub const fn turn_failed(&self) -> u32 {
        self.turn.failed
    }

    /// How many of the current turn's calls are open and not waiting for
    /// the user.
    pub fn turn_running(&self) -> u32 {
        let running = self
            .open
            .iter()
            .filter(|&(seq, open)| seq > self.turn.start && open.phase != Phase::Asked)
            .count();
        u32::try_from(running).unwrap_or(u32::MAX)
    }
}

/// Values by seq, in seq order. A session has few of each open at a time,
/// so a vector.
#[derive(Clone, Debug)]
struct BySeq<V>(Vec<(u64, V)>);

impl<V> Default for BySeq<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> BySeq<V> {
    fn insert(&mut self, seq: u64, value: V) {
        let at = self.0.partition_point(|&(s, _)| s < seq);
        match self.0.get_mut(at) {
            Some((s, held)) if *s == seq => *held = value,
            _ => self.0.insert(at, (seq, value)),
        }
    }

    fn at(&self, seq: u64) -> Option<usize> {
        self.0.iter().position(|&(s, _)| s == seq)
    }

    fn get(&self, seq: u64) -> Option<&V> {
        self.at(seq).map(|at| &self.0[at].1)
    }

    fn get_mut(&mut self, seq: u64) -> Option<&mut V> {
        self.at(seq).map(|at| &mut self.0[at].1)
    }

    fn remove(&mut self, seq: u64) -> Option<V> {
        self.at(seq).map(|at| self.0.remove(at).1)
    }

    fn iter(&self) -> impl Iterator<Item = (u64, &V)> {
        self.0.iter().map(|(seq, value)| (*seq, value))
    }
}
