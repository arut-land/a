//! What a session's log says now, folded once per entry as it commits or
//! loads, so that no question about the log scans it: which calls are open
//! and where each stands, what waits for the user and for the agent, which
//! watches are armed, and the settings, environment and context last
//! logged. Pure: the daemon and any client fold the same entries to the
//! same ledger.

use std::collections::VecDeque;

use crate::{
    Activity, Audience, Author, Body, ClientId, ClientSeq, Entry, Environment, Installed, Json,
    Mode, Outcome, Overrides, Permission, SessionId, ToolChoice, Verdict, WaitOn,
};

#[derive(Clone, Debug, Default)]
pub struct Ledger {
    /// The seq of the last entry folded.
    seq: u64,
    /// Model and tool calls with an intent and no outcome, by seq.
    open: BySeq<Open>,
    /// Input queued and not yet taken up, in order.
    untaken: VecDeque<Queued>,
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
    /// The last mutation logged of each client that named itself.
    clients: Vec<(ClientId, ClientSeq)>,
    /// What the session last settled with, as a child hands it back.
    result: Option<Outcome>,
    /// The seq of the input that started the current turn; 0 before the
    /// first.
    turn: u64,
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
    Model {
        attempt: u16,
        choice: ToolChoice,
    },
    Tool {
        tool: String,
        input: Json,
        by: Audience,
    },
    /// A side model request on behalf of call `of`.
    Side {
        of: u64,
    },
}

/// Where an open call stands. The one lifecycle of everything that waits
/// (an approval, a dialog, a client, a child): what is running, and what
/// waits on whom, is read from it rather than kept beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Its intent is committed and its gate's decisions are being made,
    /// `at` of them so far; it runs once they are.
    Gating { at: u16 },
    /// It waits, and runs (or settles) once answered.
    Waiting(WaitOn),
    /// It runs: it started a process, reported progress or was approved.
    Running,
    /// It has an outcome its result entry is yet to log, such as a call
    /// denied or blocked.
    Settled(OutcomeKind),
}

/// What an outcome was, without what it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeKind {
    Done,
    Failed,
    Interrupted,
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
    /// no wait) changes nothing.
    pub fn apply(&mut self, e: &Entry) {
        let (seq, body) = (e.seq, &e.body);
        self.seq = seq;
        match body {
            Body::Input {
                text,
                mode,
                taken,
                origin,
            } => {
                self.author(*origin);
                if *taken {
                    self.turn = seq;
                } else {
                    self.untaken.push_back(Queued {
                        seq,
                        text: text.clone(),
                        steer: *mode == Mode::Steer,
                    });
                }
            }
            Body::Take { input } => {
                if let Some(at) = self.untaken.iter().position(|q| q.seq == input.get())
                    && let Some(q) = self.untaken.remove(at)
                    && !q.steer
                {
                    self.turn = seq;
                }
            }
            Body::ToolCall { .. }
            | Body::ModelCall { .. }
            | Body::ToolProcess { .. }
            | Body::Progress { .. }
            | Body::ToolResult { .. }
            | Body::ModelResult { .. }
            | Body::Decision { .. }
            | Body::Wait { .. }
            | Body::Answer { .. } => self.track(seq, body),
            Body::Interrupt { origin } => {
                self.author(*origin);
                self.result = Some(Outcome::Interrupted { output: None });
            }
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
                    output: Json::encode(text.as_str()).unwrap_or_default(),
                });
            }
            Body::Environment(env) => self.env = Some(env.clone()),
            Body::Extensions { installed } => self.extensions = Some(installed.clone()),
            Body::Context { .. } => self.context = true,
            // The settings extension's model, until it folds its own state.
            Body::Ext { ext, state, data } if ext == "settings" && state == "model" => {
                let model = data.parse().unwrap_or_default();
                self.settings = Some((seq, Overrides { model }));
            }
            Body::Ext { .. } | Body::Edit { .. } | Body::Unknown { .. } => {}
        }
    }

    /// Folds an entry about a call: its intent, processes, progress,
    /// outcome, a decision on it, what it waits on and the answer.
    fn track(&mut self, seq: u64, body: &Body) {
        match body {
            Body::ToolCall {
                tool, input, by, ..
            } => {
                self.open(
                    seq,
                    Call::Tool {
                        tool: tool.clone(),
                        input: input.clone(),
                        by: *by,
                    },
                    Phase::Gating { at: 0 },
                );
            }
            Body::ModelCall {
                attempt,
                choice,
                r#for,
                ..
            } => {
                let call = match r#for {
                    Some(of) => Call::Side { of: of.get() },
                    None => Call::Model {
                        attempt: *attempt,
                        choice: *choice,
                    },
                };
                self.open(seq, call, Phase::Running);
            }
            Body::ToolProcess {
                call,
                pgid,
                boot,
                background,
            } => {
                if let Some(open) = self.open.get_mut(*call) {
                    open.processes.push((*pgid, boot.clone()));
                    open.ran();
                }
                if *background {
                    self.background.push((*pgid, boot.clone()));
                }
            }
            Body::Progress { call, text } => {
                if let Some(open) = self.open.get_mut(*call) {
                    open.progress.get_or_insert_default().push_str(text);
                    open.ran();
                }
            }
            Body::ToolResult { call, outcome, .. } | Body::ModelResult { call, outcome } => {
                self.settle(*call, outcome);
            }
            Body::Decision {
                call: Some(call),
                d,
                ..
            } => self.decide(call.get(), d),
            Body::Wait { call, on } => self.wait(call.get(), on),
            Body::Answer {
                call,
                value,
                origin,
            } => {
                self.author(*origin);
                self.answer(call.get(), value);
            }
            _ => {}
        }
    }

    fn open(&mut self, seq: u64, call: Call, phase: Phase) {
        self.open.insert(
            seq,
            Open {
                call,
                phase,
                processes: Vec::new(),
                progress: None,
            },
        );
    }

    fn settle(&mut self, call: u64, outcome: &Outcome) {
        let Some(open) = self.open.remove(call) else {
            return;
        };
        // A command the user ran is not the agent's work, nor is a side
        // request an extension made.
        if !open.by_user() && !matches!(open.call, Call::Side { .. }) {
            self.result = Some(outcome.clone());
        }
    }

    /// A decision about call `call`, made at its gate.
    fn decide(&mut self, call: u64, d: &Verdict) {
        let Some(open) = self.open.get_mut(call) else {
            return;
        };
        // A retried model call's attempts are counted here, so a restart
        // does not start the count again.
        if let (Verdict::Retry { attempt, .. }, Call::Model { attempt: at, .. }) =
            (d, &mut open.call)
        {
            *at = (*at).max(*attempt);
            return;
        }
        match open.phase {
            Phase::Waiting(WaitOn::Approval) => {
                if let Verdict::Approval { approved } = d {
                    self.answered(call, *approved);
                }
            }
            Phase::Gating { at } => {
                open.phase = match d {
                    Verdict::Permission {
                        permission: Permission::Ask,
                    } => Phase::Waiting(WaitOn::Approval),
                    Verdict::Permission {
                        permission: Permission::Deny,
                    }
                    | Verdict::Block { .. }
                    | Verdict::Approval { approved: false } => Phase::Settled(OutcomeKind::Failed),
                    _ => Phase::Gating {
                        at: at.saturating_add(1),
                    },
                };
            }
            _ => {}
        }
    }

    /// Call `call` waits on `on`. A tool call waits for an approval only
    /// while gated, so an answered one is not asked again.
    fn wait(&mut self, call: u64, on: &WaitOn) {
        let Some(open) = self.open.get_mut(call) else {
            return;
        };
        let may = match on {
            WaitOn::Approval => matches!(open.phase, Phase::Gating { .. }),
            _ => matches!(open.phase, Phase::Gating { .. } | Phase::Running),
        };
        if may && matches!(open.call, Call::Tool { .. }) {
            open.phase = Phase::Waiting(on.clone());
        }
    }

    /// The first answer to what call `call` waits on wins.
    fn answer(&mut self, call: u64, value: &Json) {
        let Some(open) = self.open.get_mut(call) else {
            return;
        };
        match open.phase {
            Phase::Waiting(WaitOn::Approval) => {
                self.answered(call, value.parse().unwrap_or_default());
            }
            Phase::Waiting(_) => open.phase = Phase::Running,
            _ => {}
        }
    }

    fn answered(&mut self, call: u64, approved: bool) {
        if let Some(open) = self.open.get_mut(call) {
            open.phase = if approved {
                Phase::Running
            } else {
                Phase::Settled(OutcomeKind::Failed)
            };
        }
    }

    fn author(&mut self, origin: Option<Author>) {
        let Some(Author { client, seq }) = origin else {
            return;
        };
        match self.clients.iter_mut().find(|(c, _)| *c == client) {
            Some((_, last)) => *last = (*last).max(seq),
            None => self.clients.push((client, seq)),
        }
    }

    /// The seq of the last entry folded: how many the log has.
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// The open model and tool calls the agent made, by seq: commands the
    /// user ran aside ([`Ledger::open_commands`]).
    pub fn open_calls(&self) -> impl Iterator<Item = (u64, &Open)> {
        self.open.iter().filter(|(_, open)| !open.by_user())
    }

    pub fn call(&self, seq: u64) -> Option<&Open> {
        self.open.get(seq)
    }

    /// The tool calls waiting for an approval, by seq: the session's
    /// inbox.
    pub fn inbox(&self) -> impl Iterator<Item = u64> + '_ {
        self.open
            .iter()
            .filter(|(_, open)| open.phase == Phase::Waiting(WaitOn::Approval))
            .map(|(seq, _)| seq)
    }

    pub fn untaken(&self) -> &VecDeque<Queued> {
        &self.untaken
    }

    /// Commands the user ran with an intent and no outcome, by seq.
    pub fn open_commands(&self) -> impl Iterator<Item = u64> + '_ {
        self.open
            .iter()
            .filter(|(_, open)| open.by_user())
            .map(|(seq, _)| seq)
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

    /// The number of client `client`'s last mutation logged: a mutation of
    /// its numbered no higher is a resend.
    pub fn last_of(&self, client: ClientId) -> Option<ClientSeq> {
        self.clients
            .iter()
            .find(|(c, _)| *c == client)
            .map(|&(_, seq)| seq)
    }

    /// What the session last settled with: its last answer or outcome.
    pub const fn result(&self) -> Option<&Outcome> {
        self.result.as_ref()
    }

    /// The seq of the current turn's input; 0 before the first.
    pub const fn turn_start(&self) -> u64 {
        self.turn
    }

    /// What the session is doing, as its log says: what waits on someone
    /// first, then whether anything runs or waits to.
    pub fn activity(&self) -> Activity {
        let waits = |on: fn(&WaitOn) -> bool| {
            self.open
                .iter()
                .any(|(_, open)| matches!(&open.phase, Phase::Waiting(w) if on(w)))
        };
        if waits(|w| *w == WaitOn::Approval) {
            Activity::Approval
        } else if waits(|w| matches!(w, WaitOn::Dialog { .. })) {
            Activity::Input
        } else if waits(|w| matches!(w, WaitOn::Client { .. })) {
            Activity::Client
        } else if self.open.iter().next().is_some()
            || !self.untaken.is_empty()
            || self.armed.iter().next().is_some()
        {
            Activity::Running
        } else {
            Activity::Idle
        }
    }
}

impl Open {
    fn by_user(&self) -> bool {
        matches!(
            self.call,
            Call::Tool {
                by: Audience::User,
                ..
            }
        )
    }

    /// It is seen running: a process or progress of a gated call says its
    /// gate let it through.
    fn ran(&mut self) {
        if let Phase::Gating { .. } = self.phase {
            self.phase = Phase::Running;
        }
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
