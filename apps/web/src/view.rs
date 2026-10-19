//! The pages and the pieces of a transcript a live tab patches in, as
//! markup. Every transcript item renders inside an element of its own
//! (`#i{key}`) so that a change replaces just that item; the transcript's
//! end (`#end`) follows the items, and new items go in before it.
use hypertext::prelude::*;
use protocol::view::{
    Approval, Item, Phase, SessionView, Status, Usage, call_title, decision_line, output_text,
    recap_line,
};
use protocol::{Card, Json, Outcome, SessionId, SessionSummary};

use crate::{http, style};

/// Renders `markup` to a string.
pub fn html(markup: impl Renderable) -> String {
    markup.render().into_inner()
}

/// Lays an item's own elements out as the transcript's, so the element
/// that names it does not change the layout.
const ITEM: &str = "display: contents;";

#[component]
fn document<'a, R: Renderable>(title: &'a str, children: &R) -> impl Renderable {
    maud! {
        !DOCTYPE
        html lang="en" style=(style::ROOT) {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                script type="module" src=(http::SCRIPT_PATH) {}
            }
            body style=(style::BODY) { (children) }
        }
    }
}

/// The list of sessions, or why there is none.
pub fn sessions_page(listed: &Result<Vec<SessionSummary>, String>) -> String {
    html(maud! {
        Document title="a · sessions" {
            header style=(style::BAR) {
                span style=(style::BRAND) { "a" }
                span style=(style::DIM) { "sessions" }
                form method="post" action="/new" style="margin: 0 0 0 auto;" {
                    button type="submit" style=(style::BUTTON) { "New session" }
                }
            }
            main style=(style::MAIN) {
                div style=(style::COLUMN) {
                    @match listed {
                        Ok(sessions) => {
                            @if sessions.is_empty() {
                                p style=(style::NOTE) { "No sessions yet." }
                            }
                            @for summary in sessions {
                                a style=(style::CARD) href={ "/s/" (format_args!("{}", summary.id)) } {
                                    span style=(style::MONO) { (format_args!("{}", summary.id.short())) }
                                    span style="flex: 1; min-width: 12rem; overflow-wrap: anywhere;" {
                                        (format_args!("{}", summary.workspace.display()))
                                    }
                                    span style=(style::DIM) {
                                        (summary.entries) " entries"
                                        @if summary.clients > 0 {
                                            " · " (summary.clients) " attached"
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            p style=(style::BAD) { "Could not reach the daemon: " (e) }
                        }
                    }
                }
            }
        }
    })
}

/// A chat page for browser tab `tab`: its transcript arrives over the
/// tab's event stream, and what the tab sends names the tab.
pub fn chat_page(session: SessionId, tab: &str) -> String {
    let title = format!("a · {}", session.short());
    html(maud! {
        Document title=(&title) {
            header style=(style::BAR) data-signals={ "{tab: '" (tab) "', draft: ''}" } {
                a style=(style::BRAND) href="/" { "← a" }
                span style=(style::MONO) { (format_args!("{}", session.short())) }
            }
            main style=(style::SCROLLER)
                data-init={ "@get('/s/" (format_args!("{session}")) "/events?tab=" (tab) "', {openWhenHidden: true})" } {
                div #transcript style=(style::COLUMN) {
                    p style=(style::NOTE) { "Connecting…" }
                }
            }
            div style=(style::COMPOSER_BAR) {
                form style=(style::COMPOSER) data-on:submit="$draft.trim() && @post('/api/send')" {
                    input style=(style::INPUT) name="text" placeholder="Message" autocomplete="off"
                        autofocus data-bind:draft;
                    button type="submit" style=(style::BUTTON) { "Send" }
                }
            }
        }
    })
}

/// The transcript's end: Stop while a turn runs, the daemon's latest
/// complaint, and whether it went away.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct End {
    pub running: bool,
    pub error: Option<String>,
    pub ended: bool,
}

#[component]
pub fn end_row<'a>(end: &'a End) -> impl Renderable {
    maud! {
        div #end style=(ITEM) {
            @if end.running {
                div style=(style::ASK) {
                    button style=(style::BUTTON_QUIET) data-on:click="@post('/api/interrupt')" { "Stop" }
                }
            }
            @if let Some(error) = &end.error {
                p style=(style::BAD) { (error) }
            }
            @if end.ended {
                p style=(style::WARN) { "The daemon closed the connection; reload to reconnect." }
            }
        }
    }
}

/// The whole transcript as `view` holds it.
#[component]
pub fn transcript<'a>(view: &'a SessionView, end: &'a End) -> impl Renderable {
    maud! {
        div #transcript style=(style::COLUMN) {
            @if view.items.is_empty() {
                p style=(style::NOTE) { "No messages yet." }
            }
            @if let Some(recap) = &view.recap {
                div style=(style::RECAP) { (recap_line(recap)) }
            }
            @for item in &view.items {
                ItemRow item=(item) view=(view);
            }
            EndRow end=(end);
        }
    }
}

/// One transcript item, with the reply streaming into it if it is `view`'s.
#[component]
pub fn item_row<'a>(item: &'a Item, view: &'a SessionView) -> impl Renderable {
    maud! {
        div #{ "i" (item.key()) } style=(ITEM) {
            @match item {
                Item::User { text, .. } => {
                    div style=(style::MINE) { (text) }
                }
                Item::Assistant { text, .. } => {
                    div style=(style::THEIRS) { (text) }
                }
                Item::Reply { call, text, status, usage, .. } => {
                    ReplyRow call=(*call) text=(text) status=(status) usage=(usage.as_ref())
                        live=(view.streaming.as_ref().filter(|s| s.call == *call).map(|s| s.text.as_str()));
                }
                Item::Tool { call, tool, input, approval, outcome, card, .. } => {
                    ToolCard call=(*call) line=(&call_title(tool, input, card.as_ref())) approval=(*approval)
                        phase=(item.phase().unwrap_or(Phase::Running)) outcome=(outcome.as_ref()) card=(card.as_ref());
                }
                Item::Command { name, args, outcome, .. } => {
                    div style=(style::NOTE) {
                        span style=(style::MONO) { "/" (name) " " (args) }
                        @if let Some(outcome) = outcome {
                            " · " (outcome_text(outcome))
                        }
                    }
                }
                Item::Hook { hook, decision, .. } => {
                    p style=(style::NOTE) { "hook " (hook) " · " (decision_line(decision)) }
                }
                Item::Child { child, input, fired, .. } => {
                    div style=(style::NOTE) {
                        "subagent "
                        a style=(style::LINK) href={ "/s/" (format_args!("{child}")) } {
                            (format_args!("{}", child.short()))
                        }
                        " · " (input)
                        @if let Some(outcome) = fired {
                            " · " (outcome_text(outcome))
                        }
                    }
                }
                Item::Environment { cwd, .. } => {
                    p style=(style::NOTE) { "environment · " (format_args!("{}", cwd.display())) }
                }
            }
        }
    }
}

/// Where the text streamed for model call `call` goes.
pub fn reply_id(call: u64) -> String {
    format!("#reply-{call}")
}

/// A piece of streamed text, as appended to its reply.
pub fn delta(text: &str) -> String {
    html(maud! { span { (text) } })
}

/// A model call: its streamed text while it runs, then the committed reply.
#[component]
fn reply_row<'a>(
    call: u64,
    text: &'a str,
    status: &'a Status,
    usage: Option<&'a Usage>,
    live: Option<&'a str>,
) -> impl Renderable {
    maud! {
        @match status {
            Status::Running => {
                @if let Some(text) = live.filter(|t| !t.is_empty()) {
                    div style=(style::THEIRS) {
                        span #{ "reply-" (call) } { (text) } "▍"
                    }
                } @else {
                    p style=(style::NOTE) { "thinking…" }
                }
            }
            Status::Done => {
                @if !text.is_empty() {
                    div style=(style::THEIRS) { (text) }
                }
                @if let Some(Usage { input, output }) = usage {
                    p style=(style::NOTE) { (input) " in · " (output) " out" }
                }
            }
            Status::Failed { error } => {
                p style=(style::BAD) { "model failed: " (error) }
            }
            Status::Interrupted => {
                p style=(style::WARN) { "model call interrupted" }
            }
        }
    }
}

/// A tool call: what it runs, the approval it waits for, and its outcome.
#[component]
fn tool_card<'a>(
    call: u64,
    line: &'a str,
    approval: Option<Approval>,
    phase: Phase,
    outcome: Option<&'a Outcome>,
    card: Option<&'a Card>,
) -> impl Renderable {
    maud! {
        div style=(style::TOOL) {
            div style=(style::TOOL_HEAD) {
                span style=(style::MONO) { (line) }
                @match approval {
                    Some(Approval::Approved) => { span style=(style::OK) { "approved" } }
                    Some(Approval::Denied) => { span style=(style::BAD) { "not approved" } }
                    _ => {}
                }
                @if phase == Phase::Running {
                    span style=(style::DIM) { "running…" }
                }
            }
            @if phase == Phase::Asking {
                div style=(style::ASK) {
                    span { "Run this?" }
                    button style=(style::BUTTON)
                        data-on:click={ "@post('/api/approve?call=" (call) "&ok=1')" } { "Approve" }
                    button style=(style::BUTTON_QUIET)
                        data-on:click={ "@post('/api/approve?call=" (call) "&ok=0')" } { "Deny" }
                }
            }
            @if phase == Phase::Waiting {
                div style=(style::ASK) { span style=(style::DIM) { "waiting for approval" } }
            }
            @match outcome {
                Some(Outcome::Done { output }) => {
                    OutputBlock output=(output) card=(card);
                }
                Some(Outcome::Failed { error }) => {
                    pre style=(style::OUTPUT) { span style=(style::BAD) { (error) } }
                }
                Some(Outcome::Interrupted { .. }) => {
                    pre style=(style::OUTPUT) { span style=(style::WARN) { "interrupted before it finished" } }
                }
                None => {}
            }
        }
    }
}

/// A finished call's output: as its tool presents it, a command's streams
/// and exit status as older logs hold them, anything else as JSON.
#[component]
fn output_block<'a>(output: &'a Json, card: Option<&'a Card>) -> impl Renderable {
    let command = output_text(output, card);
    maud! {
        @if let Some((text, note)) = &command {
            pre style=(style::OUTPUT) { (text) span style=(style::DIM) { (note) } }
        } @else {
            pre style=(style::OUTPUT) {
                (serde_json::to_string_pretty(&output.value()).unwrap_or_default())
            }
        }
    }
}

fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done { output } => output.to_string(),
        Outcome::Failed { error } => format!("failed: {error}"),
        Outcome::Interrupted { .. } => "interrupted".to_owned(),
    }
}
