//! The pages and the pieces of a transcript a live tab patches in, as
//! markup. Every transcript item renders inside an element of its own
//! (`#i{key}`) so that a change replaces just that item; the transcript's
//! end (`#end`) follows the items, and new items go in before it. What an
//! item says is [`Item::present`]'s; here is only where it goes.
use hypertext::prelude::*;
use protocol::view::md::{self, Block, Inline};
use protocol::view::{Glyph, Item, Kind, Presented, SessionView, Shown, recap_line};
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
                                        (format_args!("{}", summary.workspace))
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
            header style=(style::BAR) data-signals={ "{tab: '" (tab) "', draft: '', admitted: false}" } {
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
                form style=(style::COMPOSER)
                    data-effect="if ($admitted !== false) { if ($draft === $admitted) $draft = ''; $admitted = false; }"
                    data-on:submit="$draft.trim() && @post('/api/send')" {
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
    let p = view.present(item);
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
                    ToolCard p=(&p) session=(view.session);
                }
                Kind::Command if !client::approval_parts(&p).is_empty() => {
                    ToolCard p=(&p) session=(view.session);
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
                        OutputLink body=(&p.body) session=(view.session);
                    }
                }
            }
        }
    }
}

/// The active reply is independent of the committed range being transferred.
#[component]
pub fn live_reply<'a>(call: CallId, text: &'a str) -> impl Renderable {
    maud! {
        div #live {
            div style=(style::THEIRS) { span #{ "live-reply-" (call.get()) } { (text) } "▍" }
        }
    }
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
fn tool_card<'a>(p: &'a Presented<'a>, session: Option<SessionId>) -> impl Renderable {
    maud! {
        div style=(style::TOOL) {
            div style=(style::TOOL_HEAD) {
                span style=(style::MONO) { (p.title) }
                span style=(status_style(p.glyph)) { (p.status) }
            }
            @if let (Some(question), Some(id), protocol::view::Buttons::Approve(call)) = (p.question, p.question_seq, p.buttons) {
                form novalidate data-call=(call.get()) data-question=(id.get())
                    onsubmit="event.preventDefault(); const f=event.currentTarget,values={}; for(const e of f.querySelectorAll('[data-field]')) {const include=f.querySelector('[name=include_'+e.name+']');if(include&&!include.checked)continue;if(e.tagName==='SELECT'&&!e.multiple&&e.selectedOptions[0]?.disabled)continue;values[e.name]=e.multiple?JSON.stringify(Array.from(e.selectedOptions,x=>x.value)):e.value;} fetch('/api/answer',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({tab:f.querySelector('[name=tab]').value,call:Number(f.dataset.call),question:Number(f.dataset.question),action:event.submitter.value,values})});" {
                    input type="hidden" name="tab" data-attr:value="$tab";
                    p { "Requested by " (question.source) }
                    @for (pi, prompt) in question.prompts.values().enumerate() {
                        p { (prompt.message()) }
                        @if question.prompts.len() > 1 {
                            label { "Response for this prompt "
                                select name=(format!("p{pi}")) data-field { option value="accept" { "Accept" } option value="decline" { "Decline" } option value="cancel" { "Cancel" } }
                            }
                        }
                        @match prompt {
                            protocol::Prompt::Url { url, destination, .. } => {
                                p { strong { "Destination: " (destination.origin) } }
                                @if let Some(raw) = &destination.raw_origin { p { "Raw origin: " code { (raw) } } }
                                @if destination.suspicious { p role="alert" { "Warning: internationalized or confusable host. Inspect both spellings before opening." } }
                                pre style=(style::OUTPUT) { (url) }
                                a href=(url) target="_blank" rel="noopener noreferrer" referrerpolicy="no-referrer" { "Open URL in browser" }
                            }
                            protocol::Prompt::Form { fields, .. } => {
                                @for (fi, field) in fields.iter().enumerate() {
                                    div {
                                        @if !field.required {
                                            label { input type="checkbox" name=(format!("include_f{pi}_{fi}")) checked[field.default.is_some()]; "Include optional field " }
                                        }
                                        label for=(format!("field_{}_{}_{}",id.get(),pi,fi)) { (field.title.as_deref().unwrap_or(&field.name)) @if field.required { " (required)" } }
                                        @if let Some(description) = &field.description { p { (description) } }
                                        @if field.sensitive {
                                            p role="alert" { "Warning: " (question.source) " is requesting sensitive information. Secrets belong in a secure browser flow; you can decline." }
                                        }
                                        @if field.sensitive && !matches!(field.kind, protocol::FieldKind::Choice { .. }) {
                                            input id=(format!("field_{}_{}_{}",id.get(),pi,fi)) name=(format!("f{pi}_{fi}")) data-field type="password" autocomplete="off" value=(field.default_text().unwrap_or_default());
                                        } @else {
                                            @match &field.kind {
                                                protocol::FieldKind::Boolean => {
                                                    select id=(format!("field_{}_{}_{}",id.get(),pi,fi)) name=(format!("f{pi}_{fi}")) data-field {
                                                        option value="" { "Choose…" } option value="true" selected[field.default.as_ref().is_some_and(|v| v.raw()=="true")] { "Yes" } option value="false" selected[field.default.as_ref().is_some_and(|v| v.raw()=="false")] { "No" }
                                                    }
                                                }
                                                protocol::FieldKind::Choice { .. } => { (question_choices(field, id, pi, fi)) }
                                                kind => {
                                                    input id=(format!("field_{}_{}_{}",id.get(),pi,fi)) name=(format!("f{pi}_{fi}")) data-field
                                                        type=(if matches!(kind,protocol::FieldKind::Number { .. }) {"number"} else {"text"}) step="any"
                                                        value=(field.default_text().unwrap_or_default());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    button type="submit" value="accept" { "Submit / Continue" }
                    button type="submit" value="decline" { "Decline" }
                    button type="submit" value="cancel" { "Cancel" }
                }
            }
            @for part in client::approval_parts(p) {
                @match part {
                    client::ApprovalPart::Call { text } => { pre style=(style::OUTPUT) { (text) } }
                    client::ApprovalPart::Reason { text } | client::ApprovalPart::UnverifiedSummary { text } => {
                        pre style=(style::NOTE) { (text) }
                    }
                    client::ApprovalPart::Controls { call, .. } => {
                        div style=(style::ASK) {
                            span { "Run this?" }
                            button style=(style::BUTTON)
                                data-on:click={ "@post('/api/approve?call=" (call.get()) "&ok=1')" } { "Approve" }
                            button style=(style::BUTTON_QUIET)
                                data-on:click={ "@post('/api/approve?call=" (call.get()) "&ok=0')" } { "Deny" }
                        }
                    }
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
            OutputLink body=(&p.body) session=(session);
        }
    }
}

#[component]
fn output_link<'a>(body: &'a Shown<'a>, session: Option<SessionId>) -> impl Renderable {
    maud! {
        @if let (Shown::Held(output), Some(session)) = (body, session) {
            a style=(style::LINK) href={ "/output/" (format_args!("{session}")) "/" (format_args!("{}", output.id)) } target="_blank" { (protocol::view::OPEN_OUTPUT) }
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

#[component]
fn question_choices(
    field: &protocol::Field,
    id: protocol::Seq,
    pi: usize,
    fi: usize,
) -> impl Renderable {
    let control = maud! {
        @if let protocol::FieldKind::Choice { options, selection } = &field.kind {
            select id=(format!("field_{}_{}_{}", id.get(), pi, fi)) name=(format!("f{pi}_{fi}")) data-field
                multiple[matches!(selection, protocol::Selection::Many { .. })]
                onblur="const d=this.closest('details');if(d)d.open=false"
                onchange="const d=this.closest('details');if(d){d.querySelector('summary').textContent=Array.from(this.selectedOptions).some(o=>!o.disabled)?'••••':'Choose options';if(!this.multiple)d.open=false;}" {
                @if matches!(selection, protocol::Selection::One) { option value="" disabled selected[field.default.is_none()] { "Choose…" } }
                @for option in options {
                    option value=(option.value) selected[field.default.as_ref().is_some_and(|v| v.parse::<serde_json::Value>().is_ok_and(|v| v.as_str()==Some(option.value.as_str()) || v.as_array().is_some_and(|vs| vs.iter().any(|v| v.as_str()==Some(option.value.as_str())))))] { (option.title) }
                }
            }
        }
    };
    maud! {
        @if field.sensitive {
            details {
                summary { @if field.default.is_some() { "••••" } @else { "Choose options" } }
                (control)
            }
        } @else { (control) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{DaemonMsg, Decode};

    #[test]
    fn child_output_links_retrieve_content_without_exposing_storage_labels() {
        let session = SessionId::new();
        let child = SessionId::new();
        let output = protocol::BlobRef {
            id: protocol::BlobId([7; 32]),
            size: 12000,
            media_type: "text/plain".into(),
            origin: protocol::BlobOrigin::ToolOutput(CallId::new(1).unwrap()),
        };
        let item = Item::Child {
            call: CallId::new(1).unwrap(),
            child,
            input: "child work".into(),
            watch: None,
            fired: Some(protocol::Outcome::Done {
                output: protocol::Payload::Blob(Box::new(output.clone())),
            }),
        };
        let view =
            SessionView::from_log(session, "/w".into(), std::iter::empty::<protocol::Entry>());
        let rendered = html(item_row(&item, &view));
        assert!(rendered.contains(&format!("href=\"/s/{child}\"")));
        assert!(rendered.contains(&format!("href=\"/output/{session}/{}\"", output.id)));
        assert!(rendered.contains("Open output"));
        assert!(rendered.contains("12000 bytes"));
        assert!(!rendered.contains("blob"));
    }

    #[test]
    fn question_urls_are_explicit_links_and_answers_use_a_post_body() {
        let mut view = SessionView::default();
        view.fold(&DaemonMsg::Entry(protocol::Entry::new(
            protocol::Seq::new(1).unwrap(),
            protocol::Epoch(1),
            protocol::Body::Question {
                question: protocol::Question {
                    source: "server <untrusted>".into(),
                    prompts: std::collections::BTreeMap::from([(
                        "url".into(),
                        protocol::Prompt::Url {
                            message: "Continue".into(),
                            destination: protocol::Destination {
                                origin: "https://example.com".into(),
                                raw_origin: None,
                                suspicious: false,
                            },
                            url: "https://example.com/authorize?secret=opaque&next=1".into(),
                        },
                    )]),
                },
            },
        )));
        let rendered = html(ItemRow {
            item: &view.items[0],
            view: &view,
        });
        assert!(rendered.contains("server &lt;untrusted&gt;"));
        assert!(rendered.contains("noopener noreferrer"));
        assert!(rendered.contains("no-referrer"));
        assert!(rendered.contains("fetch('/api/answer'"));
        assert!(!rendered.contains("window.open"));
        assert!(!rendered.contains("/api/answer?"));
    }

    #[test]
    fn sensitive_inputs_are_password_controls_and_answers_never_render_in_history() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../crates/protocol/tests/vectors/view.json"
        ))
        .unwrap();
        let vector = vectors
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "sensitive answers never enter client history")
            .unwrap();
        let mut view = SessionView::default();
        let mut seen = false;
        for message in vector["frames"].as_array().unwrap() {
            view.fold(&DaemonMsg::from_json(message.as_str().unwrap().as_bytes()).unwrap());
            for item in &view.items {
                let rendered = html(ItemRow { item, view: &view });
                if rendered.contains("type=\"password\"") {
                    seen = true;
                    assert!(
                        rendered.contains("misleading fixture is requesting sensitive information")
                    );
                    assert!(rendered.contains("https://аpple.com"));
                    assert!(rendered.contains("https://xn--pple-43d.com"));
                    assert!(rendered.contains("confusable host"));
                    assert!(!rendered.contains("window.open"));
                }
                assert!(!rendered.contains("fixture-secret-never-shown"));
            }
        }
        assert!(seen);
    }

    #[test]
    fn sensitive_choices_keep_opaque_values_and_titles_behind_a_masked_summary() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../crates/protocol/tests/vectors/view.json"
        ))
        .unwrap();
        let vector = vectors
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "sensitive choices retain titled options")
            .unwrap();
        let mut view = SessionView::default();
        for message in vector["frames"].as_array().unwrap() {
            view.fold(&DaemonMsg::from_json(message.as_str().unwrap().as_bytes()).unwrap());
        }
        let rendered = html(ItemRow {
            item: &view.items[0],
            view: &view,
        });
        assert_eq!(rendered.matches("<details>").count(), 2);
        assert_eq!(
            rendered
                .matches("<summary>Choose options</summary>")
                .count(),
            2
        );
        assert_eq!(
            rendered
                .matches("value=\"opaque-read\">Read access</option>")
                .count(),
            2
        );
        assert_eq!(
            rendered
                .matches("value=\"opaque-write\">Write access</option>")
                .count(),
            2
        );
        assert!(rendered.contains("name=\"f0_0\""));
        assert!(rendered.contains("name=\"f0_1\" data-field multiple"));
        assert!(!rendered.contains("type=\"password\""));
        assert!(rendered.contains("'••••'"));
    }

    #[test]
    fn every_summary_has_complete_effective_input_above_it_and_controls_below() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../crates/protocol/tests/vectors/view.json"
        ))
        .unwrap();
        for vector in vectors
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["name"].as_str().unwrap().starts_with("approval reviews "))
        {
            let mut view = SessionView::default();
            for message in vector["messages"].as_array().unwrap() {
                view.fold(&DaemonMsg::from_json(message.to_string().as_bytes()).unwrap());
            }
            let rendered = html(ItemRow {
                item: &view.items[0],
                view: &view,
            });
            let reason = rendered.find("Why:").unwrap();
            let summary = rendered.find("Model summary (unverified):").unwrap();
            let controls = rendered.find("Run this?").unwrap();
            assert!(reason < summary && summary < controls);
            let markers: &[&str] = if vector["name"].as_str().unwrap().contains("multiline") {
                &[
                    "printf harmless\nrm -rf /tmp/approval-test-sentinel",
                    "60000",
                    "every argument",
                ]
            } else {
                &[
                    "/work/deep/config.ini",
                    "-allow = false",
                    "+allow = true",
                    "-delete = no",
                    "+delete = yes",
                    "flag",
                ]
            };
            for marker in markers {
                assert!(
                    rendered.find(marker).unwrap() < reason,
                    "missing full input: {marker}: {rendered}"
                );
            }
        }
    }
}
