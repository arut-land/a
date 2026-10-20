//! The pages and the pieces of a transcript a live tab patches in, as
//! markup. Every transcript item renders inside an element of its own
//! (`#i{key}`) so that a change replaces just that item; the transcript's
//! end (`#end`) follows the items, and new items go in before it. What an
//! item says is [`Item::present`]'s; here is only where it goes.
use hypertext::prelude::*;
use protocol::view::md::{self, Block, Inline};
use protocol::view::{Buttons, Glyph, Item, Kind, Presented, SessionView, Shown, recap_line};
use protocol::{CallId, SessionId, SessionSummary};

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
                TitleSpan title=(None);
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

/// The session's title, once its model named it.
#[component]
pub fn title_span<'a>(title: Option<&'a str>) -> impl Renderable {
    maud! {
        span #title style=(style::DIM) { (title.unwrap_or_default()) }
    }
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

/// The style of a status class.
const fn status_style(glyph: Glyph) -> &'static str {
    match glyph {
        Glyph::None | Glyph::Busy => style::DIM,
        Glyph::Ok => style::OK,
        Glyph::Bad | Glyph::Denied => style::BAD,
        Glyph::Warn | Glyph::Ask => style::WARN,
    }
}

/// One transcript item, with the reply streaming into it if it is `view`'s.
#[component]
pub fn item_row<'a>(item: &'a Item, view: &'a SessionView) -> impl Renderable {
    let p = item.present();
    let live = match item {
        Item::Reply { call, .. } => (view.streaming.as_ref())
            .filter(|s| s.call == *call)
            .map(|s| s.text.as_str()),
        _ => None,
    };
    maud! {
        div #{ "i" (item.key().get()) } style=(ITEM) {
            @match p.kind {
                Kind::User => {
                    div style=(style::MINE) { (p.body.text()) }
                }
                Kind::Assistant => {
                    div style=(style::THEIRS) { (Markdown { text: &p.body.text() }) }
                }
                Kind::Reply => {
                    ReplyRow key=(item.key()) p=(&p) live=(live);
                }
                // A side request's reply is its extension's, not the conversation's:
                // a title shows in the bar.
                Kind::Side => {}
                Kind::Tool => {
                    ToolCard p=(&p);
                }
                Kind::Command => {
                    div style=(style::NOTE) {
                        span style=(style::MONO) { (p.title) }
                        " · " span style=(status_style(p.glyph)) { (p.status) }
                        @if p.body != Shown::None {
                            " · " (p.body.text())
                        }
                    }
                }
                Kind::Hook | Kind::Environment => {
                    p style=(style::NOTE) { (p.title) }
                }
                Kind::Child => {
                    div style=(style::NOTE) {
                        @if let Item::Child { child, .. } = item {
                            a style=(style::LINK) href={ "/s/" (format_args!("{child}")) } { (p.title) }
                        }
                        " · " (p.body.text())
                        " · " span style=(status_style(p.glyph)) { (p.status) }
                    }
                }
            }
        }
    }
}

/// Where the text streamed for model call `call` goes.
pub fn reply_id(call: CallId) -> String {
    format!("#reply-{call}")
}

/// A piece of streamed text, as appended to its reply.
pub fn delta(text: &str) -> String {
    html(maud! { span { (text) } })
}

/// A model call: its streamed text while it runs, then the committed reply.
#[component]
fn reply_row<'a>(
    key: protocol::Seq,
    p: &'a Presented<'a>,
    live: Option<&'a str>,
) -> impl Renderable {
    maud! {
        @match p.glyph {
            Glyph::Busy => {
                @if let Some(text) = live.filter(|t| !t.is_empty()) {
                    div style=(style::THEIRS) {
                        span #{ "reply-" (key.get()) } { (text) } "▍"
                    }
                } @else {
                    p style=(style::NOTE) { "thinking…" }
                }
            }
            Glyph::None => {
                @if let Shown::Markdown(text) = p.body && !text.is_empty() {
                    div style=(style::THEIRS) { (Markdown { text }) }
                }
                @if let Some(usage) = p.usage {
                    p style=(style::NOTE) { (format_args!("{usage}")) }
                }
            }
            glyph => {
                p style=(status_style(glyph)) { (p.status) }
            }
        }
    }
}

/// A tool call: what it runs, where it stands, and what it shows for it.
#[component]
fn tool_card<'a>(p: &'a Presented<'a>) -> impl Renderable {
    maud! {
        div style=(style::TOOL) {
            div style=(style::TOOL_HEAD) {
                span style=(style::MONO) { (p.title) }
                span style=(status_style(p.glyph)) { (p.status) }
            }
            @if let Buttons::Approve(call) = p.buttons {
                div style=(style::ASK) {
                    span { "Run this?" }
                    button style=(style::BUTTON)
                        data-on:click={ "@post('/api/approve?call=" (call.get()) "&ok=1')" } { "Approve" }
                    button style=(style::BUTTON_QUIET)
                        data-on:click={ "@post('/api/approve?call=" (call.get()) "&ok=0')" } { "Deny" }
                }
            }
            @match &p.body {
                Shown::None => {}
                Shown::Json(output) => {
                    // Any tool's output, shown as a tree of it prints.
                    pre style=(style::OUTPUT) {
                        (output.parse::<serde_json::Value>().and_then(|tree| serde_json::to_string_pretty(&tree)).unwrap_or_default())
                    }
                }
                body => {
                    pre style=(style::OUTPUT) { (body.text()) }
                }
            }
        }
    }
}

/// A reply's markdown: the blocks as elements, bullets and code lines
/// grouped, inline bold and code as such.
#[component]
fn markdown<'a>(text: &'a str) -> impl Renderable {
    let groups = group(text);
    maud! {
        @for g in &groups {
            @match g {
                Group::Heading(level, text) => {
                    @match level {
                        1 => h1 { (Inlines { text }) }
                        2 => h2 { (Inlines { text }) }
                        _ => h3 { (Inlines { text }) }
                    }
                }
                Group::Bullets(items) => {
                    ul { @for text in items { li { (Inlines { text }) } } }
                }
                Group::Quote(lines) => {
                    blockquote { @for text in lines { (Inlines { text }) "\n" } }
                }
                Group::Code(lines) => {
                    pre { code { @for line in lines { (line) "\n" } } }
                }
                Group::Para(text) => {
                    p { (Inlines { text }) }
                }
            }
        }
    }
}

#[component]
fn inlines<'a>(text: &'a str) -> impl Renderable {
    maud! {
        @for run in md::inlines(text) {
            @match run {
                Inline::Text(t) => { (t) }
                Inline::Bold(t) => { strong { (t) } }
                Inline::Code(t) => { code { (t) } }
            }
        }
    }
}

/// Lines that go in one element together.
enum Group<'a> {
    Heading(u8, &'a str),
    Bullets(Vec<&'a str>),
    Quote(Vec<&'a str>),
    Code(Vec<&'a str>),
    Para(&'a str),
}

fn group(text: &str) -> Vec<Group<'_>> {
    let mut groups: Vec<Group<'_>> = Vec::new();
    for block in md::blocks(text) {
        match (block, groups.last_mut()) {
            (Block::Bullet { text, .. }, Some(Group::Bullets(items))) => items.push(text),
            (Block::Bullet { text, .. }, _) => groups.push(Group::Bullets(vec![text])),
            (Block::Quote(text), Some(Group::Quote(lines))) => lines.push(text),
            (Block::Quote(text), _) => groups.push(Group::Quote(vec![text])),
            (Block::Code(line), Some(Group::Code(lines))) => lines.push(line),
            (Block::Code(line), _) => groups.push(Group::Code(vec![line])),
            // An empty fence still shows as one.
            (Block::Fence(_), Some(Group::Code(_))) => {}
            (Block::Fence(_), _) => groups.push(Group::Code(Vec::new())),
            (Block::Heading(level, text), _) => groups.push(Group::Heading(level, text)),
            (Block::Para(text), _) if !text.trim().is_empty() => groups.push(Group::Para(text)),
            (Block::Para(_), _) => {}
        }
    }
    groups
}
