//! How an item is shown, decided once: every renderer (a terminal, a web
//! page, a line printer, a native app) lays a [`Presented`] out in its own
//! way but says the same thing in the same words.

use std::borrow::Cow;

use crate::ledger::Phase;
use crate::view::{Item, Recap, Status, Usage};
use crate::{
    ApprovalReason, Audience, CallId, Card, Json, Mark, Outcome, Permission, Verdict, WaitOn,
};

/// What an item is, for the layout a renderer gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Assistant,
    /// A model call of the conversation.
    Reply,
    /// A side request an extension made: not the conversation's.
    Side,
    Tool,
    /// A slash command the user ran.
    Command,
    Hook,
    Child,
    Environment,
}

/// The class of an item's status: how it is coloured, and the sign a text
/// renderer puts before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Glyph {
    #[default]
    None,
    Ok,
    Bad,
    Warn,
    Busy,
    /// Waits on the user.
    Ask,
    /// Refused before it ran.
    Denied,
}

impl Glyph {
    /// The sign a text renderer shows for it; empty for none.
    pub const fn sign(self) -> &'static str {
        match self {
            Self::None | Self::Busy => "",
            Self::Ok => "✓",
            Self::Bad => "✗",
            Self::Warn => "⚠",
            Self::Ask => "?",
            Self::Denied => "⊘",
        }
    }

    const fn of(mark: Mark) -> Self {
        match mark {
            Mark::Ok => Self::Ok,
            Mark::Bad => Self::Bad,
            Mark::Warn => Self::Warn,
        }
    }
}

/// What goes under an item's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown<'a> {
    None,
    /// Preformatted text.
    Text(Cow<'a, str>),
    /// A unified diff.
    Diff(&'a str),
    /// A tool's output no card presents, shown as the tree it is.
    Json(&'a Json),
    /// A reply's text.
    Markdown(&'a str),
}

impl Shown<'_> {
    /// The text, as a renderer that shows no structure prints it.
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            Self::None => Cow::Borrowed(""),
            Self::Text(text) => Cow::Borrowed(text),
            Self::Diff(text) | Self::Markdown(text) => Cow::Borrowed(text),
            Self::Json(json) => Cow::Owned(json.to_string()),
        }
    }
}

/// The controls an item offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Buttons {
    None,
    /// Approve or deny call `call`, which waits for it.
    Approve(CallId),
}

/// An item as every renderer shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presented<'a> {
    pub kind: Kind,
    pub glyph: Glyph,
    /// One line: what it is, such as `$ ls`, `/model gpt` or `child 1a2b`;
    /// empty for a message, which is all body.
    pub title: Cow<'a, str>,
    /// Where it stands, such as `exit 0` or `waiting for approval`; empty
    /// when it has nothing to say.
    pub status: Cow<'a, str>,
    pub body: Shown<'a>,
    /// A reply's token counts, once it has them.
    pub usage: Option<Usage>,
    pub buttons: Buttons,
    pub reason: Option<&'a ApprovalReason>,
    pub description: Option<&'a str>,
    pub input: Option<(&'a str, &'a Json)>,
}

/// An item with nothing to say of its state.
fn plain<'a>(kind: Kind, title: Cow<'a, str>, body: Shown<'a>) -> Presented<'a> {
    Presented {
        kind,
        glyph: Glyph::None,
        title,
        status: Cow::Borrowed(""),
        body,
        usage: None,
        buttons: Buttons::None,
        reason: None,
        description: None,
        input: None,
    }
}

impl Item {
    /// How this item is shown.
    pub fn present(&self) -> Presented<'_> {
        match self {
            Self::User { text, .. } => plain(Kind::User, "".into(), Shown::Text(text.into())),
            Self::Assistant { text, .. } => {
                plain(Kind::Assistant, "".into(), Shown::Markdown(text))
            }
            Self::Reply {
                model,
                text,
                status,
                usage,
                purpose,
                ..
            } => replied(model, text, status, *usage, purpose.as_deref()),
            Self::Tool {
                call,
                tool,
                input,
                phase,
                outcome,
                card,
                by,
                reason,
                ..
            } => {
                let (kind, title) = match by {
                    Audience::User => (
                        Kind::Command,
                        Cow::Owned(
                            format!("/{tool} {}", Self::args(input))
                                .trim_end()
                                .to_owned(),
                        ),
                    ),
                    _ => (Kind::Tool, call_title(tool, input, card.as_ref())),
                };
                let mut p = plain(kind, title, Shown::None);
                standing(&mut p, *call, phase, outcome.as_ref(), card.as_ref());
                p.reason = reason.as_deref();
                p.input = Some((tool, input));
                p
            }
            Self::Hook {
                hook,
                call,
                tool,
                decision,
                ..
            } => {
                let line = decision_line(tool, *call, decision);
                plain(
                    Kind::Hook,
                    format!("hook {hook}: {line}").into(),
                    Shown::None,
                )
            }
            Self::Child {
                child,
                input,
                fired,
                ..
            } => {
                let title = format!("child {}", child.short());
                let mut p = plain(Kind::Child, title.into(), Shown::Text(input.into()));
                (p.glyph, p.status) = match fired {
                    None => (Glyph::Busy, "running…".into()),
                    Some(Outcome::Done { .. }) => (Glyph::Ok, "done".into()),
                    Some(Outcome::Failed { error }) => {
                        (Glyph::Bad, format!("failed: {error}").into())
                    }
                    Some(Outcome::Interrupted { .. }) => (Glyph::Warn, "interrupted".into()),
                };
                p
            }
            Self::Environment { cwd, .. } => plain(
                Kind::Environment,
                format!("env {}", cwd.display()).into(),
                Shown::None,
            ),
        }
    }
}

/// A model call's reply: the conversation's, or with a `purpose`, a side
/// request's.
fn replied<'a>(
    model: &'a str,
    text: &'a str,
    status: &'a Status,
    usage: Option<Usage>,
    purpose: Option<&'a str>,
) -> Presented<'a> {
    let (kind, title) = match purpose {
        Some(purpose) => (Kind::Side, Cow::Borrowed(purpose)),
        None => (Kind::Reply, Cow::Borrowed(model)),
    };
    let mut p = plain(kind, title, Shown::None);
    match status {
        Status::Running => p.glyph = Glyph::Busy,
        Status::Done => {
            p.body = Shown::Markdown(text);
            p.usage = usage;
        }
        Status::Failed { error } => {
            p.glyph = Glyph::Bad;
            p.status = format!("model failed: {error}").into();
        }
        Status::Interrupted => {
            p.glyph = Glyph::Warn;
            p.status = "interrupted · what should it do instead?".into();
        }
    }
    p
}

/// Where a tool call or command stands, and what it shows for it: the one
/// decision table over its phase, outcome and card.
fn standing<'a>(
    p: &mut Presented<'a>,
    call: CallId,
    phase: &Phase,
    outcome: Option<&'a Outcome>,
    card: Option<&'a Card>,
) {
    let Some(outcome) = outcome else {
        let (glyph, status) = match phase {
            Phase::Waiting(WaitOn::Approval) => {
                p.buttons = Buttons::Approve(call);
                (Glyph::Ask, "waiting for approval")
            }
            Phase::Waiting(WaitOn::Dialog { .. }) => (Glyph::Ask, "waiting for input"),
            Phase::Waiting(WaitOn::Client { .. }) => (Glyph::Ask, "waiting for a client"),
            Phase::Settled(_) => (Glyph::Denied, "not approved"),
            Phase::Gating { .. } | Phase::Running | Phase::Waiting(WaitOn::Child { .. }) => {
                (Glyph::Busy, "running…")
            }
        };
        (p.glyph, p.status) = (glyph, status.into());
        return;
    };
    match outcome {
        Outcome::Done { output } => {
            let presented = card.filter(|c| !c.body.is_empty() || !c.note.is_empty());
            p.glyph = Glyph::of(card.map_or(Mark::Ok, |c| c.mark));
            p.status = match presented {
                Some(card) if !card.note.is_empty() => Cow::Borrowed(card.note.as_str()),
                _ => Cow::Borrowed("done"),
            };
            p.body = match presented {
                Some(card) => {
                    let text = card.body.strip_suffix('\n').unwrap_or(&card.body);
                    if text.is_empty() {
                        Shown::None
                    } else if text.starts_with("@@ ") {
                        Shown::Diff(text)
                    } else {
                        Shown::Text(text.into())
                    }
                }
                None if output.raw() == "null" => Shown::None,
                None => match output.parse::<String>() {
                    Ok(text) => Shown::Text(text.into()),
                    Err(_) => Shown::Json(output),
                },
            };
        }
        Outcome::Failed { error } => {
            p.glyph = Glyph::Bad;
            p.status = "failed".into();
            p.body = Shown::Text(error.into());
        }
        Outcome::Interrupted { output } => {
            p.glyph = Glyph::Warn;
            p.status = "interrupted · what should it do instead?".into();
            p.body = match output.as_deref().map(|t| t.trim_end_matches('\n')) {
                Some(text) if !text.is_empty() => Shown::Text(text.into()),
                _ => Shown::None,
            };
        }
    }
}

/// A tool call in one line: as its tool presents it, else a command as
/// `$ command` and anything else as the tool and its input.
pub fn call_title<'a>(tool: &'a str, input: &'a Json, card: Option<&'a Card>) -> Cow<'a, str> {
    if let Some(card) = card {
        return Cow::Borrowed(&card.title);
    }
    match super::command(input) {
        Some(command) => format!("$ {command}").into(),
        None => format!("{tool} {input}").into(),
    }
}

/// What a client says of the answer a call waited for.
pub const fn answer_line(approved: bool) -> &'static str {
    if approved { "approved" } else { "not approved" }
}

/// What a client says when the daemon turns out to speak protocol
/// `supported`, not this build's.
pub fn mismatch_line(supported: u32) -> String {
    format!("the daemon speaks protocol {supported}; restart it")
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
pub fn decision_line(tool: &str, call: Option<CallId>, decision: &Verdict) -> String {
    let call = call.map_or(0, CallId::get);
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
                Permission::Ask(_) => "ask",
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
        Verdict::SideRetry { attempt, .. } => {
            format!("retrying side call {call}, attempt {attempt}")
        }
        Verdict::Retry { attempt, error } => {
            format!("retried call {call} (attempt {attempt}): {error}")
        }
    }
}
