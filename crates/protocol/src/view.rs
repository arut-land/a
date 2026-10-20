//! Shared reducers: the state every client (the CLI, the web UI, the apps
//! through their FFI facade) folds daemon messages into, so all of them show
//! the same session.
//!
//! The log holds the facts; everything here is derived from them. A
//! [`SessionView`]'s serialized form is its log-derived state: identical for
//! any two clients that have folded the same session up to the same seq,
//! which [`SessionView::state_hash`] checks. What only this connection saw
//! (streamed text, the recap of what it missed) is kept beside it and left
//! out.

use std::borrow::Borrow;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Audience, Body, Card, DaemonMsg, Entry, Environment, Json, Outcome, Permission, SessionId,
    SessionSummary, Verdict, WaitOn,
};

/// One session as a client shows it. Its serialized form is also what a
/// [`DaemonMsg::Snapshot`] carries.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SessionView {
    pub session: Option<SessionId>,
    pub workspace: PathBuf,
    /// The last entry folded: a reconnect attaches with it as `since`.
    pub seq: u64,
    /// The transcript, in log order.
    pub items: Vec<Item>,
    /// The environment the session's commands run in.
    pub environment: Option<Environment>,
    /// Input waiting to be taken up, by the seq of its entry: shown once
    /// it is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<(u64, String)>,
    /// The session's title: the latest reply to a side request titling it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What happened since the seq the latest attach started from, once
    /// its replay is in.
    #[serde(skip)]
    pub recap: Option<Recap>,
    /// The seq the latest attach started from.
    #[serde(skip)]
    pub(crate) since: u64,
    /// Live text of the model call in flight; the committed reply replaces
    /// it.
    #[serde(skip)]
    pub streaming: Option<Stream>,
    /// Counts the updates [`SessionView::fold`] returned. An update whose
    /// `rev` is not one more than the last seen means some were dropped (an
    /// FFI stream's buffer was full): take a fresh snapshot of the view.
    #[serde(skip)]
    pub rev: u64,
}

/// What happened in a session, for a client that attaches: what its turn
/// did since the user's last message, as [`SessionView::recap_since`]
/// reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recap {
    /// The user's last message, shortened.
    pub prompt: Option<String>,
    /// The tools called since, in order.
    pub tools: Vec<String>,
    /// Calls since that failed or were interrupted.
    pub failed: u32,
    /// Calls since with no outcome yet.
    pub running: u32,
    /// The agent's last reply since, shortened.
    pub reply: Option<String>,
    /// Calls waiting for the user's approval.
    pub asks: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub call: u64,
    /// The attempt the text is of: a retry streams anew.
    pub attempt: u16,
    pub text: String,
}

/// A transcript item, keyed by the seq of the entry that started it (for a
/// call, its call id).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    User {
        seq: u64,
        text: String,
    },
    Assistant {
        seq: u64,
        text: String,
    },
    /// A model call and the reply it got; a side request an extension
    /// made names its `purpose`.
    Reply {
        call: u64,
        model: String,
        text: String,
        status: Status,
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        purpose: Option<String>,
    },
    /// A call of a tool, by the model or (`by` the user) a slash command,
    /// whose arguments are its input's `args` ([`Item::args`]).
    Tool {
        call: u64,
        tool: String,
        input: Json,
        approval: Option<Approval>,
        outcome: Option<Outcome>,
        /// How the tool presents the call, or once it has one, its outcome.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        card: Option<Card>,
        #[serde(default, skip_serializing_if = "crate::decode::skip")]
        by: Audience,
    },
    /// What extension `hook` decided, about call `call` (of tool `tool`)
    /// when it is about one; an approval asked is the call's [`Approval`].
    Hook {
        seq: u64,
        hook: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<u64>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tool: String,
        decision: Verdict,
    },
    /// A child session started by call `call`, and its watch.
    Child {
        call: u64,
        child: SessionId,
        input: String,
        watch: Option<u64>,
        fired: Option<Outcome>,
    },
    Environment {
        seq: u64,
        cwd: PathBuf,
    },
}

impl Item {
    /// Where the call this item shows stands; `None` for an item that is
    /// not a call.
    pub const fn phase(&self) -> Option<Phase> {
        Some(match self {
            Self::Reply { status, .. } => match status {
                Status::Running => Phase::Running,
                Status::Done => Phase::Done,
                Status::Failed { .. } => Phase::Failed,
                Status::Interrupted => Phase::Interrupted,
            },
            Self::Tool {
                outcome: Some(outcome),
                ..
            } => Phase::of(outcome),
            Self::Tool {
                approval: Some(Approval::Pending),
                ..
            } => Phase::Asking,
            Self::Tool {
                approval: Some(Approval::Denied),
                ..
            } => Phase::Denied,
            Self::Tool { .. } => Phase::Running,
            _ => return None,
        })
    }

    /// The seq of the entry that started this item.
    pub const fn key(&self) -> u64 {
        match self {
            Self::User { seq, .. }
            | Self::Assistant { seq, .. }
            | Self::Hook { seq, .. }
            | Self::Environment { seq, .. } => *seq,
            Self::Reply { call, .. } | Self::Tool { call, .. } | Self::Child { call, .. } => *call,
        }
    }

    /// The arguments of a slash command, as typed after its name: its
    /// input's `args`.
    pub fn args(input: &Json) -> String {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            args: String,
        }
        input.parse::<Args>().map_or_default(|a| a.args)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Running,
    Done,
    Failed { error: String },
    Interrupted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Pending,
    Approved,
    Denied,
}

/// Where a model call, tool call or slash command stands: the decision
/// table over an item's status, approval and outcome, kept here once so
/// that no client re-derives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// It runs, or is about to.
    Running,
    /// It waits for an approval, which every client attached is asked for.
    Asking,
    /// Not approved; it will not run.
    Denied,
    Done,
    Failed,
    Interrupted,
}

impl Phase {
    /// Whether it is unsettled: what an interrupt stops. A call waiting for
    /// approval counts, since an interrupt settles it without running it.
    pub const fn open(self) -> bool {
        matches!(self, Self::Running | Self::Asking)
    }

    /// Whether it has its outcome: nothing about it changes any more.
    pub const fn settled(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Interrupted)
    }

    const fn of(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Done { .. } => Self::Done,
            Outcome::Failed { .. } => Self::Failed,
            Outcome::Interrupted { .. } => Self::Interrupted,
        }
    }
}

/// What a session's turn is doing, as [`SessionView::turn`] finds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Turn {
    /// A call is unsettled: an interrupt has something to stop.
    pub running: bool,
    /// The oldest call waiting for an approval.
    pub asking: Option<u64>,
    /// How many calls wait for an approval.
    pub asks: u32,
}

/// What one daemon message changed, as owned data a client applies to its
/// own copy: items are upserted whole by index.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Update {
    /// One more than the previous update's: a gap means updates were lost.
    pub rev: u64,
    /// The view's seq after this update.
    pub seq: u64,
    pub change: Change,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum Change {
    /// Attached to `session`; `reset` when it is not the one shown before,
    /// so the view started over.
    Attached {
        session: SessionId,
        workspace: PathBuf,
        reset: bool,
    },
    Added {
        index: usize,
        item: Item,
    },
    Updated {
        index: usize,
        item: Item,
        part: Part,
    },
    /// Live text for the reply of `call`, appended to [`SessionView::streaming`].
    Delta {
        call: u64,
        text: String,
    },
    /// The replay is in; [`SessionView::recap`] tells what it held.
    Synced,
    /// The daemon sent the whole view: the items are these now.
    Snapshot {
        items: Vec<Item>,
    },
    Draining,
    Mismatch {
        supported: u32,
    },
    Error {
        message: String,
    },
}

/// Which part of an item an [`Change::Updated`] changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    /// A tool's result, a reply's, a command's, or a child's fired watch.
    Outcome,
    Approval,
    Watch,
}

impl SessionView {
    /// The view of `session` as its log `entries` leave it: what a client
    /// that attached and folded them would hold, but for what only a
    /// connection sees.
    pub fn from_log(
        session: SessionId,
        workspace: PathBuf,
        entries: impl IntoIterator<Item = impl Borrow<Entry>>,
    ) -> Self {
        let mut view = Self {
            session: Some(session),
            workspace,
            ..Self::default()
        };
        for e in entries {
            view.entry(e.borrow());
        }
        view
    }

    /// Folds one daemon message in. `None` when it changed nothing, such as
    /// an entry replayed again after a reconnect, or a kind this build does
    /// not know.
    pub fn fold(&mut self, msg: &DaemonMsg) -> Option<Update> {
        let change = self.apply(msg)?;
        self.rev += 1;
        Some(Update {
            rev: self.rev,
            seq: self.seq,
            change,
        })
    }

    /// Tool calls waiting for an approval, oldest first.
    pub fn asked(&self) -> impl Iterator<Item = u64> + '_ {
        self.items
            .iter()
            .filter(|item| item.phase() == Some(Phase::Asking))
            .map(Item::key)
    }

    /// Where the session's turn stands, from the items' phases: the one
    /// place any client learns whether something runs and what waits on
    /// it.
    pub fn turn(&self) -> Turn {
        let mut turn = Turn::default();
        for item in &self.items {
            match item.phase() {
                Some(Phase::Asking) => {
                    turn.running = true;
                    turn.asks += 1;
                    turn.asking = turn.asking.or(Some(item.key()));
                }
                Some(phase) if phase.open() => turn.running = true,
                _ => {}
            }
        }
        turn
    }

    /// A hash of the log-derived state (the serialized view): equal on every
    /// client that folded the same session to the same seq. FNV-1a, so it is
    /// the same on every platform and build.
    pub fn state_hash(&self) -> u64 {
        let mut hash = Fnv(0xcbf2_9ce4_8422_2325);
        serde_json::to_writer(&mut hash, self).expect("a view always serializes");
        hash.0
    }

    fn apply(&mut self, msg: &DaemonMsg) -> Option<Change> {
        match msg {
            DaemonMsg::Attached { session, workspace } => {
                let reset = self.session != Some(*session);
                if reset {
                    *self = Self {
                        session: Some(*session),
                        rev: self.rev,
                        ..Self::default()
                    };
                }
                self.since = self.seq;
                self.recap = None;
                self.workspace.clone_from(workspace);
                Some(Change::Attached {
                    session: *session,
                    workspace: workspace.clone(),
                    reset,
                })
            }
            DaemonMsg::Entry(e) => self.entry(e),
            DaemonMsg::Delta {
                call,
                text,
                attempt,
            } => {
                match &mut self.streaming {
                    Some(s) if s.call == *call && s.attempt == *attempt => s.text.push_str(text),
                    s => {
                        *s = Some(Stream {
                            call: *call,
                            attempt: *attempt,
                            text: text.clone(),
                        });
                    }
                }
                Some(Change::Delta {
                    call: *call,
                    text: text.clone(),
                })
            }
            DaemonMsg::Synced { seq } => {
                self.seq = self.seq.max(*seq);
                self.recap = self.recap_since(self.since);
                Some(Change::Synced)
            }
            DaemonMsg::Draining => Some(Change::Draining),
            DaemonMsg::Mismatch { supported, .. } => Some(Change::Mismatch {
                supported: *supported,
            }),
            DaemonMsg::Error { message } => Some(Change::Error {
                message: message.clone(),
            }),
            DaemonMsg::Rejected {
                reason, message, ..
            } => Some(Change::Error {
                message: if message.is_empty() {
                    format!("refused: {reason:?}")
                } else {
                    message.clone()
                },
            }),
            DaemonMsg::Snapshot(view) => Some(self.adopt(view)),
            // Skipped, but a reconnect still starts after it.
            DaemonMsg::Unknown { seq: Some(seq) } => {
                self.seq = self.seq.max(*seq);
                None
            }
            DaemonMsg::Welcome { .. }
            | DaemonMsg::Sessions { .. }
            | DaemonMsg::Settings { .. }
            | DaemonMsg::Status { .. }
            | DaemonMsg::Ack { .. }
            | DaemonMsg::Unknown { .. } => None,
        }
    }

    /// Takes the log-derived state of `snapshot`, keeping what only this
    /// connection saw where it still holds: the reply streaming in.
    fn adopt(&mut self, snapshot: &Self) -> Change {
        let items = snapshot.items.clone();
        let running = |call| {
            items.iter().any(
                |i| matches!(i, Item::Reply { call: c, status: Status::Running, .. } if *c == call),
            )
        };
        if self.streaming.as_ref().is_some_and(|s| !running(s.call)) {
            self.streaming = None;
        }
        self.session = snapshot.session;
        self.workspace.clone_from(&snapshot.workspace);
        self.seq = snapshot.seq;
        self.environment.clone_from(&snapshot.environment);
        self.queued.clone_from(&snapshot.queued);
        self.title.clone_from(&snapshot.title);
        self.items = items;
        Change::Snapshot {
            items: self.items.clone(),
        }
    }

    /// Folds a new entry: most start an item, the rest change one.
    fn entry(&mut self, e: &Entry) -> Option<Change> {
        if e.seq <= self.seq {
            return None;
        }
        self.seq = e.seq;
        let seq = e.seq;
        let item = match &e.body {
            Body::Input {
                text, taken: true, ..
            } => Item::User {
                seq,
                text: text.clone(),
            },
            Body::Input { text, .. } => {
                self.queued.push((seq, text.clone()));
                return None;
            }
            Body::Take { input } => {
                let at = self.queued.iter().position(|(s, _)| *s == input.get())?;
                let (_, text) = self.queued.remove(at);
                Item::User { seq, text }
            }
            Body::ToolCall {
                tool,
                input,
                by: Audience::User,
                ..
            } => return Some(self.command(seq, tool, input)),
            Body::Decision {
                call: Some(call),
                d:
                    Verdict::Permission {
                        permission: Permission::Ask,
                    },
                ..
            } => return self.approval(call.get(), Approval::Pending),
            Body::Decision { by, call, d } => {
                let call = call.map(crate::CallId::get);
                Item::Hook {
                    seq,
                    hook: by.clone(),
                    call,
                    tool: self.tool_of(call),
                    decision: d.clone(),
                }
            }
            Body::Assistant { text } => Item::Assistant {
                seq,
                text: text.clone(),
            },
            Body::ToolCall {
                tool,
                input,
                card,
                by,
                ..
            } => Item::Tool {
                call: seq,
                tool: tool.clone(),
                input: input.clone(),
                approval: None,
                outcome: None,
                card: card.clone(),
                by: *by,
            },
            Body::ModelCall { model, purpose, .. } => Item::Reply {
                call: seq,
                model: model.clone(),
                text: String::new(),
                status: Status::Running,
                usage: None,
                purpose: purpose.clone(),
            },
            Body::Link { child, input } => Item::Child {
                call: seq,
                child: *child,
                input: input.clone(),
                watch: None,
                fired: None,
            },
            Body::Environment(env) => {
                self.environment = Some(env.clone());
                Item::Environment {
                    seq,
                    cwd: env.cwd.clone(),
                }
            }
            body => return self.change(seq, body),
        };
        self.items.push(item.clone());
        Some(Change::Added {
            index: self.items.len() - 1,
            item,
        })
    }

    /// Folds entry `seq`, which changes an item already shown.
    fn change(&mut self, seq: u64, body: &Body) -> Option<Change> {
        match body {
            Body::ToolResult {
                call,
                outcome,
                card,
            } => {
                let at = self.call(*call)?;
                let Item::Tool {
                    outcome: o,
                    card: shown,
                    ..
                } = &mut self.items[at]
                else {
                    return None;
                };
                *o = Some(outcome.clone());
                if card.is_some() {
                    shown.clone_from(card);
                }
                Some(self.updated(at, Part::Outcome))
            }
            Body::ModelResult { call, outcome } => self.replied(*call, outcome),
            Body::Wait {
                call,
                on: WaitOn::Approval,
            } => self.approval(call.get(), Approval::Pending),
            Body::Answer { call, value, .. } => {
                let approved = value.parse::<bool>().ok()?;
                let to = if approved {
                    Approval::Approved
                } else {
                    Approval::Denied
                };
                self.approval(call.get(), to)
            }
            Body::Watch { child } => {
                let at = self.items.iter().rposition(
                    |item| matches!(item, Item::Child { child: c, watch: None, .. } if c == child),
                )?;
                if let Item::Child { watch, .. } = &mut self.items[at] {
                    *watch = Some(seq);
                }
                Some(self.updated(at, Part::Watch))
            }
            Body::Fired { watch, outcome } => {
                let at = self.items.iter().rposition(
                    |item| matches!(item, Item::Child { watch: Some(w), .. } if w == watch),
                )?;
                if let Item::Child { fired, .. } = &mut self.items[at] {
                    *fired = Some(outcome.clone());
                }
                Some(self.updated(at, Part::Outcome))
            }
            _ => None,
        }
    }

    /// A command the user ran, called `seq`. The input that ran it is
    /// shown as the command it is.
    fn command(&mut self, seq: u64, tool: &str, input: &Json) -> Change {
        let item = Item::Tool {
            call: seq,
            tool: tool.to_owned(),
            input: input.clone(),
            approval: None,
            outcome: None,
            card: None,
            by: Audience::User,
        };
        let typed = |text: &str| {
            let rest = text.strip_prefix('/').unwrap_or_default();
            rest.split_whitespace().next() == Some(tool)
        };
        if let Some(Item::User { text, .. }) = self.items.last()
            && typed(text)
        {
            let at = self.items.len() - 1;
            self.items[at] = item;
            return self.updated(at, Part::Outcome);
        }
        self.items.push(item.clone());
        Change::Added {
            index: self.items.len() - 1,
            item,
        }
    }

    /// The tool of call `call`, as a hook item names it.
    fn tool_of(&self, call: Option<u64>) -> String {
        match call
            .and_then(|call| self.call(call))
            .map(|at| &self.items[at])
        {
            Some(Item::Tool { tool, .. }) => tool.clone(),
            _ => String::new(),
        }
    }

    /// What the replay after `since` held, as a recap tells it: the user's
    /// last message and what the turn did since, `None` when nothing worth
    /// telling is new (an environment, say).
    pub fn recap_since(&self, since: u64) -> Option<Recap> {
        if self.seq <= since {
            return None;
        }
        let prompt = self
            .items
            .iter()
            .rposition(|i| matches!(i, Item::User { .. }));
        let mut recap = Recap {
            prompt: prompt.and_then(|at| match &self.items[at] {
                Item::User { text, .. } => Some(short(text)),
                _ => None,
            }),
            asks: u32::try_from(self.asked().count()).unwrap_or(u32::MAX),
            ..Recap::default()
        };
        for item in &self.items[prompt.map_or(0, |at| at + 1)..] {
            match item {
                Item::Tool {
                    tool,
                    by: Audience::Model,
                    ..
                } => recap.tools.push(tool.clone()),
                Item::Assistant { text, .. } => recap.reply = Some(short(text)),
                Item::Reply {
                    text,
                    status: Status::Done,
                    purpose: None,
                    ..
                } if !text.is_empty() => recap.reply = Some(short(text)),
                _ => {}
            }
            if let Item::Tool { .. } | Item::Reply { .. } = item {
                match item.phase() {
                    Some(Phase::Failed | Phase::Interrupted) => recap.failed += 1,
                    Some(Phase::Running) => recap.running += 1,
                    _ => {}
                }
            }
        }
        (recap != Recap::default()).then_some(recap)
    }

    fn replied(&mut self, call: u64, outcome: &Outcome) -> Option<Change> {
        if self.streaming.as_ref().is_some_and(|s| s.call == call) {
            self.streaming = None;
        }
        let at = self.call(call)?;
        let Item::Reply {
            text,
            status,
            usage,
            purpose,
            ..
        } = &mut self.items[at]
        else {
            return None;
        };
        let titles = purpose.as_deref() == Some("title");
        *status = match outcome {
            Outcome::Done { output } if titles => {
                let reply = reply_text(&output.value());
                let title = reply.trim().trim_matches('"').trim();
                self.title = (!title.is_empty()).then(|| title.to_owned());
                *text = reply;
                Status::Done
            }
            Outcome::Done { output } => {
                let output = output.value();
                *text = reply_text(&output);
                *usage = usage_of(&output);
                Status::Done
            }
            Outcome::Failed { error } => Status::Failed {
                error: error.clone(),
            },
            Outcome::Interrupted { .. } => Status::Interrupted,
        };
        Some(self.updated(at, Part::Outcome))
    }

    fn approval(&mut self, call: u64, to: Approval) -> Option<Change> {
        let at = self.call(call)?;
        match &mut self.items[at] {
            Item::Tool { approval, .. } if *approval != Some(to) => *approval = Some(to),
            _ => return None,
        }
        Some(self.updated(at, Part::Approval))
    }

    /// The index of the item call `call` started.
    fn call(&self, call: u64) -> Option<usize> {
        self.items.binary_search_by_key(&call, Item::key).ok()
    }

    fn updated(&self, index: usize, part: Part) -> Change {
        Change::Updated {
            index,
            item: self.items[index].clone(),
            part,
        }
    }
}

/// The answer text of a stored model reply: its message's text parts.
fn reply_text(output: &Value) -> String {
    output
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect()
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

fn usage_of(output: &Value) -> Option<Usage> {
    let usage = &output["usage"];
    Some(Usage {
        input: usage["input"].as_u64()?,
        output: usage["output"].as_u64()?,
    })
}

/// A tool call in one line: as its tool presents it, else a command as
/// `$ command` and anything else as the tool and its input.
pub fn call_title(tool: &str, input: &Json, card: Option<&Card>) -> String {
    if let Some(card) = card {
        return card.title.clone();
    }
    match input.value()["command"].as_str() {
        Some(command) => format!("$ {command}"),
        None => format!("{tool} {input}"),
    }
}

/// A finished call's output as text and a short note on it, as its tool's
/// card presents it; `None` when it presents neither.
pub fn output_text(card: Option<&Card>) -> Option<(String, String)> {
    card.filter(|c| !c.body.is_empty() || !c.note.is_empty())
        .map(|card| (card.body.clone(), card.note.clone()))
}

/// One line on what a session did, shown after an attach's replay.
pub fn recap_line(recap: &Recap) -> String {
    let mut parts = vec!["recap".to_owned()];
    if let Some(prompt) = &recap.prompt {
        parts.push(format!("you: {prompt}"));
    }
    match &recap.tools[..] {
        [] => {}
        few @ ([_] | [_, _] | [_, _, _]) => parts.push(format!("ran {}", few.join(", "))),
        many => parts.push(format!("ran {} tools", many.len())),
    }
    let count = |parts: &mut Vec<String>, n: u32, what: &str| {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    };
    count(&mut parts, recap.failed, "failed");
    count(&mut parts, recap.running, "running");
    if let Some(reply) = &recap.reply {
        parts.push(format!("a: {reply}"));
    }
    count(&mut parts, recap.asks, "waiting for approval");
    parts.join(" · ")
}

/// What extension decision `decision` about call `call` (of tool `tool`)
/// was, in one line.
pub fn decision_line(tool: &str, call: Option<u64>, decision: &Verdict) -> String {
    let call = call.unwrap_or_default();
    match decision {
        Verdict::Answered => format!("answered call {call} itself"),
        Verdict::PrepareStep { model: None } => format!("rewrote the request of call {call}"),
        Verdict::PrepareStep { model: Some(model) } => {
            format!("sent call {call} to {model}")
        }
        Verdict::PreTool { .. } => format!("rewrote the input of {tool}"),
        Verdict::Block { reason } => format!("blocked {tool}: {reason}"),
        Verdict::Permission { permission } => {
            let permission = match permission {
                Permission::Allow => "allow",
                Permission::Ask => "ask",
                Permission::Defer => "defer",
                Permission::Deny => "deny",
            };
            format!("{tool}: {permission}")
        }
        Verdict::Approval { approved } => {
            let answer = if *approved {
                "approved"
            } else {
                "not approved"
            };
            format!("{tool} {answer}")
        }
        Verdict::PostTool { .. } => format!("rewrote the outcome of call {call}"),
        Verdict::OnStop { text } => format!("continued with: {text}"),
        Verdict::Input { original } => format!("rewrote your input: {original}"),
        Verdict::Retry { attempt, error } => {
            format!("retried call {call} (attempt {attempt}): {error}")
        }
    }
}

/// The daemon's sessions, as `a ls` and a session picker show them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
}

impl SessionList {
    /// Whether `msg` replaced the list.
    pub fn fold(&mut self, msg: &DaemonMsg) -> bool {
        match msg {
            DaemonMsg::Sessions { sessions } => {
                self.sessions.clone_from(sessions);
                true
            }
            _ => false,
        }
    }
}

struct Fnv(u64);

impl io::Write for Fnv {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
