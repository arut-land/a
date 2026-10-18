//! How a session's transcript renders: the shared [`SessionView`] as
//! markup, emitted again whole on every update.

use protocol::view::{
    Approval, Item, SessionView, Status, Stream, Usage, call_title, decision_line, output_text,
    recap_line,
};
use protocol::{Card, Outcome};
use serde_json::Value;
use topcoat::Result;
use topcoat::runtime::Event;
use topcoat::view::{View, component, view};

use crate::app::approve;
use crate::style;

/// The transcript of `view`, as the browser tab `tab` shows it.
#[component]
pub async fn transcript_column(
    view: SessionView,
    tab: String,
    error: Option<String>,
    ended: bool,
) -> Result<impl View> {
    let SessionView {
        items,
        recap,
        streaming,
        ..
    } = view;
    let empty = items.is_empty();
    Ok(view! {
        <div style=(style::COLUMN)>
            if empty {
                <p style=(style::NOTE)>"No messages yet."</p>
            }
            if let Some(recap) = recap {
                <div style=(style::RECAP)>(recap_line(&recap))</div>
            }
            #[key(item.key())]
            for item in items {
                item_row(item: item, tab: tab.clone(), streaming: streaming.clone())
            }
            if let Some(error) = error {
                <p style=(style::BAD)>(error)</p>
            }
            if ended {
                <p style=(style::WARN)>"The daemon closed the connection; reload to reconnect."</p>
            }
        </div>
    })
}

#[component]
async fn item_row(item: Item, tab: String, streaming: Option<Stream>) -> Result<impl View> {
    Ok(view! {
        match item {
            Item::User { text, .. } => {
                <div style=(style::MINE)>(text)</div>
            }
            Item::Assistant { text, .. } => {
                <div style=(style::THEIRS)>(text)</div>
            }
            Item::Reply { call, text, status, usage, .. } => {
                reply_row(text: text, status: status, usage: usage, live: streaming.filter(|s| s.call == call).map(|s| s.text))
            }
            Item::Tool { call, tool, input, approval, outcome, card, asked } => {
                tool_card(tab: tab, call: call, line: call_title(&tool, &input, card.as_ref()), approval: approval, asked: asked, outcome: outcome, card: card)
            }
            Item::Command { name, args, outcome, .. } => {
                <div style=(style::NOTE)>
                    <span style=(style::MONO)>"/" (name) " " (args)</span>
                    if let Some(outcome) = outcome {
                        " · " (outcome_text(&outcome))
                    }
                </div>
            }
            Item::Hook { hook, decision, .. } => {
                <p style=(style::NOTE)>"hook " (hook) " · " (decision_line(&decision))</p>
            }
            Item::Child { child, input, fired, .. } => {
                <div style=(style::NOTE)>
                    "subagent "
                    <a style=(style::LINK) href=(format!("/s/{child}"))>(child.short().to_string())</a>
                    " · " (input)
                    if let Some(outcome) = fired {
                        " · " (outcome_text(&outcome))
                    }
                </div>
            }
            Item::Environment { cwd, .. } => {
                <p style=(style::NOTE)>"environment · " (cwd.display().to_string())</p>
            }
        }
    })
}

/// A model call: its streamed text while it runs, then the committed reply.
#[component]
async fn reply_row(
    text: String,
    status: Status,
    usage: Option<Usage>,
    live: Option<String>,
) -> Result<impl View> {
    Ok(view! {
        match status {
            Status::Running => {
                match live.filter(|t| !t.is_empty()) {
                    Some(text) => { <div style=(style::THEIRS)>(text) "▍"</div> }
                    None => { <p style=(style::NOTE)>"thinking…"</p> }
                }
            }
            Status::Done => {
                if !text.is_empty() {
                    <div style=(style::THEIRS)>(text)</div>
                }
                if let Some(Usage { input, output }) = usage {
                    <p style=(style::NOTE)>(input) " in · " (output) " out"</p>
                }
            }
            Status::Failed { error } => {
                <p style=(style::BAD)>"model failed: " (error)</p>
            }
            Status::Interrupted => {
                <p style=(style::WARN)>"model call interrupted"</p>
            }
        }
    })
}

/// A tool call: what it runs, the approval it waits for, and its outcome.
#[component]
async fn tool_card(
    tab: String,
    call: u64,
    line: String,
    approval: Option<Approval>,
    asked: bool,
    outcome: Option<Outcome>,
    card: Option<Card>,
) -> Result<impl View> {
    let waiting = approval == Some(Approval::Pending);
    Ok(view! {
        <div style=(style::TOOL) id=(format!("call-{call}"))>
            <div style=(style::TOOL_HEAD)>
                <span style=(style::MONO)>(line)</span>
                match approval {
                    Some(Approval::Approved) => { <span style=(style::OK)>"approved"</span> }
                    Some(Approval::Denied) => { <span style=(style::BAD)>"not approved"</span> }
                    _ => {}
                }
                if outcome.is_none() && !waiting {
                    <span style=(style::DIM)>"running…"</span>
                }
            </div>
            if waiting && asked {
                <div style=(style::ASK)>
                    <span>"Run this?"</span>
                    <button style=(style::BUTTON) @click=$(async |_e: Event| { approve(tab, call, true).await; })>"Approve"</button>
                    <button style=(style::BUTTON_QUIET) @click=$(async |_e: Event| { approve(tab, call, false).await; })>"Deny"</button>
                </div>
            }
            if waiting && !asked {
                <div style=(style::ASK)><span style=(style::DIM)>"waiting for approval"</span></div>
            }
            match outcome {
                Some(Outcome::Done { output }) => {
                    output_block(output: output, card: card)
                }
                Some(Outcome::Failed { error }) => {
                    <pre style=(style::OUTPUT)><span style=(style::BAD)>(error)</span></pre>
                }
                Some(Outcome::Interrupted { .. }) => {
                    <pre style=(style::OUTPUT)><span style=(style::WARN)>"interrupted before it finished"</span></pre>
                }
                None => {}
            }
        </div>
    })
}

/// A finished call's output: as its tool presents it, a command's streams
/// and exit status as older logs hold them, anything else as JSON.
#[component]
async fn output_block(output: Value, card: Option<Card>) -> Result<impl View> {
    let command = output_text(&output, card.as_ref());
    Ok(view! {
        match command {
            Some((text, note)) => {
                <pre style=(style::OUTPUT)>(text) <span style=(style::DIM)>(note)</span></pre>
            }
            None => {
                <pre style=(style::OUTPUT)>(serde_json::to_string_pretty(&output).unwrap_or_default())</pre>
            }
        }
    })
}

fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done { output } => output.to_string(),
        Outcome::Failed { error } => format!("failed: {error}"),
        Outcome::Interrupted { .. } => "interrupted".to_owned(),
    }
}
