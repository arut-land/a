//! Shared reducers: the state every client (the CLI, the web UI, the apps
//! through their FFI facade) folds daemon messages into, so all of them show
//! the same session.
//!
//! The log holds the facts; everything here is derived from them. A
//! [`SessionView`]'s serialized form is its log-derived state: identical for
//! any two clients that have folded the same session up to the same seq,
//! which [`SessionView::state_hash`] checks. What only this connection saw
//! (streamed text, the recap, which asks were delivered, the link state) is
//! kept beside it and left out.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Body, Card, DaemonMsg, Decision, Decode, Entry, Environment, Outcome, Recap, SessionId,
    SessionSummary,
};

/// One session as a client shows it. Its serialized form is also what a
/// [`DaemonMsg::Snapshot`] carries.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    pub session: Option<SessionId>,
    pub workspace: PathBuf,
    /// The last entry folded: a reconnect attaches with it as `since`.
    pub seq: u64,
    /// The transcript, in log order.
    pub items: Vec<Item>,
    /// The environment the session's commands run in.
    pub environment: Option<Environment>,
    #[serde(skip)]
    pub link: Link,
    /// The recap the daemon sent on the latest attach.
    #[serde(skip)]
    pub recap: Option<Recap>,
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

/// Where this client's connection to the session stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Link {
    #[default]
    Connecting,
    /// Attached; the replay of what was missed is arriving.
    Replaying,
    /// Synced: what arrives now is live.
    Live,
    /// The daemon is draining for an upgrade: reconnect and attach from
    /// [`SessionView::seq`].
    Draining,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub call: u64,
    pub text: String,
}

/// A transcript item, keyed by the seq of the entry that started it (for a
/// call, its call id).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// A model call and the reply it got.
    Reply {
        call: u64,
        model: String,
        text: String,
        status: Status,
        usage: Option<Usage>,
    },
    Tool {
        call: u64,
        tool: String,
        input: Value,
        approval: Option<Approval>,
        outcome: Option<Outcome>,
        /// How the tool presents the call, or once it has one, its outcome.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        card: Option<Card>,
        /// Whether this client was asked to approve it.
        #[serde(skip)]
        asked: bool,
    },
    /// A slash command and its outcome.
    Command {
        call: u64,
        name: String,
        args: String,
        outcome: Option<Outcome>,
    },
    /// A hook decision, other than a user's answer to an ask (which is the
    /// call's [`Approval`]).
    Hook {
        seq: u64,
        hook: String,
        decision: Decision,
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
    /// The seq of the entry that started this item.
    pub const fn key(&self) -> u64 {
        match self {
            Self::User { seq, .. }
            | Self::Assistant { seq, .. }
            | Self::Hook { seq, .. }
            | Self::Environment { seq, .. } => *seq,
            Self::Reply { call, .. }
            | Self::Tool { call, .. }
            | Self::Child { call, .. }
            | Self::Command { call, .. } => *call,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Running,
    Done,
    Failed { error: String },
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// This client is asked to approve the tool call at `index`.
    Asked {
        index: usize,
        item: Item,
    },
    Synced,
    /// The daemon sent the whole view: the items are these now.
    Snapshot {
        items: Vec<Item>,
    },
    Recap(Recap),
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
    pub fn from_log(session: SessionId, workspace: PathBuf, entries: &[Entry]) -> Self {
        let mut view = Self {
            session: Some(session),
            workspace,
            ..Self::default()
        };
        for e in entries {
            view.entry(e);
        }
        view
    }

    /// Folds one daemon message in. `None` when it changed nothing, such as
    /// an entry replayed again after a reconnect, a repeated ask, or a kind
    /// this build does not know.
    pub fn fold(&mut self, msg: &DaemonMsg) -> Option<Update> {
        let change = self.apply(msg)?;
        self.rev += 1;
        Some(Update {
            rev: self.rev,
            seq: self.seq,
            change,
        })
    }

    /// Tool calls this client was asked to approve that are still waiting,
    /// oldest first.
    pub fn asked(&self) -> impl Iterator<Item = u64> + '_ {
        self.items.iter().filter_map(|item| match item {
            Item::Tool {
                call,
                approval: Some(Approval::Pending),
                asked: true,
                ..
            } => Some(*call),
            _ => None,
        })
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
                self.workspace.clone_from(workspace);
                self.link = Link::Replaying;
                Some(Change::Attached {
                    session: *session,
                    workspace: workspace.clone(),
                    reset,
                })
            }
            DaemonMsg::Entry(e) => self.entry(e),
            DaemonMsg::Delta { call, text } => {
                match &mut self.streaming {
                    Some(s) if s.call == *call => s.text.push_str(text),
                    s => {
                        *s = Some(Stream {
                            call: *call,
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
                self.link = Link::Live;
                Some(Change::Synced)
            }
            DaemonMsg::Recap(recap) => {
                self.recap = Some(recap.clone());
                Some(Change::Recap(recap.clone()))
            }
            DaemonMsg::Ask { call, tool, input } => self.ask(*call, tool, input),
            DaemonMsg::Draining => {
                self.link = Link::Draining;
                Some(Change::Draining)
            }
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
            | DaemonMsg::Ack { .. }
            | DaemonMsg::Unknown { .. } => None,
        }
    }

    /// Takes the log-derived state of `snapshot`, keeping what only this
    /// connection saw where it still holds: which waiting calls it was asked
    /// about, and the reply streaming in.
    fn adopt(&mut self, snapshot: &Self) -> Change {
        let asked: Vec<u64> = if self.session == snapshot.session {
            self.asked().collect()
        } else {
            Vec::new()
        };
        let mut items = snapshot.items.clone();
        for item in &mut items {
            if let Item::Tool {
                call,
                approval: Some(Approval::Pending),
                asked: a,
                ..
            } = item
            {
                *a = asked.contains(call);
            }
        }
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
            Body::User { text } => Item::User {
                seq,
                text: text.clone(),
            },
            Body::Assistant { text } => Item::Assistant {
                seq,
                text: text.clone(),
            },
            Body::ToolCall {
                tool, input, card, ..
            } => Item::Tool {
                call: seq,
                tool: tool.clone(),
                input: input.clone(),
                approval: None,
                outcome: None,
                card: card.clone(),
                asked: false,
            },
            Body::Command { name, args } => Item::Command {
                call: seq,
                name: name.clone(),
                args: args.clone(),
                outcome: None,
            },
            Body::ModelCall { model } => Item::Reply {
                call: seq,
                model: model.clone(),
                text: String::new(),
                status: Status::Running,
                usage: None,
            },
            Body::Hook {
                decision: Decision::Approval { call: Some(_), .. },
                ..
            } => return self.change(seq, &e.body),
            Body::Hook { hook, decision } => Item::Hook {
                seq,
                hook: hook.clone(),
                decision: decision.clone(),
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
            Body::CommandResult { call, outcome } => {
                let at = self.call(*call)?;
                let Item::Command { outcome: o, .. } = &mut self.items[at] else {
                    return None;
                };
                *o = Some(outcome.clone());
                Some(self.updated(at, Part::Outcome))
            }
            Body::ModelResult { call, outcome } => self.replied(*call, outcome),
            Body::Ask { call } => self.approval(*call, Approval::Pending),
            Body::Hook {
                decision:
                    Decision::Approval {
                        approved,
                        call: Some(call),
                        ..
                    },
                ..
            } => self.approval(
                *call,
                if *approved {
                    Approval::Approved
                } else {
                    Approval::Denied
                },
            ),
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

    fn replied(&mut self, call: u64, outcome: &Outcome) -> Option<Change> {
        if self.streaming.as_ref().is_some_and(|s| s.call == call) {
            self.streaming = None;
        }
        let at = self.call(call)?;
        let Item::Reply {
            text,
            status,
            usage,
            ..
        } = &mut self.items[at]
        else {
            return None;
        };
        *status = match outcome {
            Outcome::Done { output } => {
                *text = reply_text(output);
                *usage = usage_of(output);
                Status::Done
            }
            Outcome::Failed { error } => Status::Failed {
                error: error.clone(),
            },
            Outcome::Interrupted { .. } => Status::Interrupted,
        };
        Some(self.updated(at, Part::Outcome))
    }

    /// The daemon asks this client to approve `call`: on the call's ask
    /// entry, and again on every attach while it waits.
    fn ask(&mut self, call: u64, tool: &str, input: &Value) -> Option<Change> {
        let at = match self.items.binary_search_by_key(&call, Item::key) {
            Ok(at) => at,
            // Attached after the call: show it from the ask.
            Err(at) => {
                let item = Item::Tool {
                    call,
                    tool: tool.to_owned(),
                    input: input.clone(),
                    approval: Some(Approval::Pending),
                    outcome: None,
                    card: None,
                    asked: false,
                };
                self.items.insert(at, item);
                at
            }
        };
        let Item::Tool {
            approval, asked, ..
        } = &mut self.items[at]
        else {
            return None;
        };
        // An answer already logged wins over a stale re-delivery.
        if *asked || approval.is_some_and(|a| a != Approval::Pending) {
            return None;
        }
        *approval = Some(Approval::Pending);
        *asked = true;
        Some(Change::Asked {
            index: at,
            item: self.items[at].clone(),
        })
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

/// The answer text of a stored model reply: its message's text parts, or
/// the bare text an earlier version logged.
fn reply_text(output: &Value) -> String {
    match output.pointer("/message/content").and_then(Value::as_array) {
        Some(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect(),
        None => output["text"].as_str().unwrap_or_default().to_owned(),
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
pub fn call_title(tool: &str, input: &Value, card: Option<&Card>) -> String {
    match (card, input["command"].as_str()) {
        (Some(card), _) => card.title.clone(),
        (None, Some(command)) => format!("$ {command}"),
        (None, None) => format!("{tool} {input}"),
    }
}

/// A finished call's output as text and a short note on it: as its tool's
/// card presents it, else a command's streams and exit status as older logs
/// hold them. `None` for any other output.
pub fn output_text(output: &Value, card: Option<&Card>) -> Option<(String, String)> {
    if let Some(card) = card.filter(|c| !c.body.is_empty() || !c.note.is_empty()) {
        return Some((card.body.clone(), card.note.clone()));
    }
    let stdout = output["stdout"].as_str()?;
    let stderr = output["stderr"].as_str().unwrap_or_default();
    Some((
        format!("{stdout}{stderr}"),
        format!("exit {}", output["exit"]),
    ))
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

impl Decode for SessionView {
    fn from_json(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
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
