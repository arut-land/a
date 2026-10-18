//! Transcript items as rows: messages, replies, and tool call cards.

use protocol::view::{Approval, Item, Status};
use protocol::{Decision, Outcome, Recap};
use serde_json::Value;

use super::render::{Color, Style, ellipsize};
use super::text::{Action, Line, Span, markdown, sanitize, wrap};

/// How much of a card's output shows unless everything is expanded.
const TAIL: usize = 10;

pub const MUTED: Style = Style::fg(Color::Gray);
pub const USER: Style = Style::fg(Color::Magenta).bold();
const OK: Style = Style::fg(Color::Green);
const BAD: Style = Style::fg(Color::Red);
const WARN: Style = Style::fg(Color::Yellow);
const BUSY: Style = Style::fg(Color::Blue);
pub const ASK: Style = Style::fg(Color::Yellow).bold();

/// How an item is shown.
#[derive(Clone, Copy, Debug, Default)]
pub struct Look<'a> {
    /// The reply text streaming in for this item, if it is that call's.
    pub streaming: Option<&'a str>,
    /// The card is folded to its header.
    pub collapsed: bool,
    /// Every card's whole output and input (Ctrl-O).
    pub verbose: bool,
}

/// Whether an item will not change any more: an inline viewport prints
/// those into the scrollback.
pub fn settled(item: &Item) -> bool {
    match item {
        Item::Reply { status, .. } => *status != Status::Running,
        Item::Tool { outcome, .. } => outcome.is_some(),
        _ => true,
    }
}

pub fn rows(item: &Item, width: usize, look: Look) -> Vec<Line> {
    let blank = Line::default();
    match item {
        Item::User { text, .. } => {
            let mut out = vec![blank];
            for (n, line) in sanitize(text).split('\n').enumerate() {
                let first = Span::new(if n == 0 { "› " } else { "  " }, USER);
                let body = [Span::new(line, Style::PLAIN.bold())];
                out.extend(wrap(&body, width, &[first], &[Span::new("  ", USER)]));
            }
            out
        }
        Item::Assistant { text, .. } => {
            let mut out = vec![blank];
            out.extend(markdown(text, width, Style::PLAIN));
            out
        }
        Item::Reply {
            model,
            text,
            status,
            ..
        } => reply(model, text, status, width, look.streaming),
        Item::Tool {
            call,
            tool,
            input,
            approval,
            outcome,
            asked,
        } => card(
            &Card {
                call: *call,
                tool,
                input,
                approval: *approval,
                outcome: outcome.as_ref(),
                asked: *asked,
            },
            width,
            look,
        ),
        Item::Hook { hook, decision, .. } => {
            vec![Line::plain(
                ellipsize(&format!("  · hook {hook}: {}", decided(decision)), width),
                MUTED,
            )]
        }
        Item::Child {
            child,
            input,
            fired,
            ..
        } => {
            let mut head = vec![
                Span::new("⑂ ", BUSY),
                Span::new(format!("child {} ", child.short()), Style::PLAIN.bold()),
            ];
            head.push(match fired {
                None => Span::new("running…", BUSY),
                Some(Outcome::Done { .. }) => Span::new("✓ done", OK),
                Some(Outcome::Failed { error }) => Span::new(format!("✗ {error}"), BAD),
                Some(Outcome::Interrupted) => Span::new("⚠ interrupted", WARN),
            });
            let mut out = vec![blank, Line::from(head)];
            let body = [Span::new(sanitize(input).replace('\n', " "), MUTED)];
            let pad = [Span::new("  ", MUTED)];
            out.extend(wrap(&body, width, &pad, &pad).into_iter().take(3));
            out
        }
        Item::Environment { cwd, .. } => {
            vec![Line::plain(
                ellipsize(&format!("  · env {}", cwd.display()), width),
                MUTED,
            )]
        }
    }
}

fn reply(
    model: &str,
    text: &str,
    status: &Status,
    width: usize,
    streaming: Option<&str>,
) -> Vec<Line> {
    let mut out = vec![Line::default()];
    match (status, streaming) {
        (Status::Running, Some(live)) => {
            out.extend(markdown(live, width, Style::PLAIN));
            if let Some(last) = out.last_mut() {
                last.spans.push(Span::new("▍", MUTED));
            }
        }
        (Status::Running, None) => out.push(Line::plain(format!("  … {model}"), MUTED)),
        (Status::Done, _) if text.is_empty() => return Vec::new(),
        (Status::Done, _) => out.extend(markdown(text, width, Style::PLAIN)),
        (Status::Failed { error }, _) => {
            let body = [Span::new(
                format!("✗ model failed: {}", sanitize(error)),
                BAD,
            )];
            out.extend(wrap(
                &body,
                width,
                &[Span::new("  ", BAD)],
                &[Span::new("    ", BAD)],
            ));
        }
        (Status::Interrupted, _) => out.push(Line::plain("  ⚠ model call interrupted", WARN)),
    }
    out
}

struct Card<'a> {
    call: u64,
    tool: &'a str,
    input: &'a Value,
    approval: Option<Approval>,
    outcome: Option<&'a Outcome>,
    asked: bool,
}

/// A tool call: a header with its state (a click folds it), its output's
/// tail, and while it waits for this client, the approval buttons.
fn card(c: &Card, width: usize, look: Look) -> Vec<Line> {
    let toggle = Action::Toggle(c.call);
    let (status, style) = match (c.outcome, c.approval) {
        (Some(Outcome::Done { output }), _) => match output["exit"].as_i64() {
            Some(0) => ("✓ exit 0".to_owned(), OK),
            Some(code) => (format!("✗ exit {code}"), BAD),
            None => ("✓ done".to_owned(), OK),
        },
        (Some(Outcome::Failed { .. }), _) => ("✗ failed".to_owned(), BAD),
        (Some(Outcome::Interrupted), _) => ("⚠ interrupted".to_owned(), WARN),
        (None, Some(Approval::Pending)) => ("? waiting for approval".to_owned(), ASK),
        (None, Some(Approval::Denied)) => ("⊘ not approved".to_owned(), BAD),
        (None, _) => ("running…".to_owned(), BUSY),
    };
    let body = output_rows(c, width, look.verbose);
    let marker = match (body.is_empty(), look.collapsed) {
        (true, _) => "• ",
        (false, true) => "▸ ",
        (false, false) => "▾ ",
    };
    let call = call_text(c.tool, c.input);
    let room = width.saturating_sub(2 + 2 + super::text::width_of(&status));
    let mut out = vec![
        Line::default(),
        Line::from(vec![
            Span::button(marker, MUTED, toggle),
            Span::button(
                ellipsize(&sanitize(&call).replace('\n', " "), room),
                Style::PLAIN.bold(),
                toggle,
            ),
            Span::button("  ", Style::PLAIN, toggle),
            Span::button(status, style, toggle),
        ]),
    ];
    if look.collapsed {
        if !body.is_empty() {
            out.push(Line::plain(format!("  │ {} lines", body.len()), MUTED));
        }
    } else {
        let hidden = if look.verbose {
            0
        } else {
            body.len().saturating_sub(TAIL)
        };
        if hidden > 0 {
            out.push(Line::from(vec![Span::button(
                format!("  │ ⋯ {hidden} earlier lines (ctrl-o shows all)"),
                MUTED,
                toggle,
            )]));
        }
        out.extend(body.into_iter().skip(hidden));
    }
    if c.outcome.is_none() && c.approval == Some(Approval::Pending) {
        out.push(if c.asked {
            Line::from(vec![
                Span::new("  allow this call?  ", ASK),
                Span::button("[ y  yes ]", OK.bold(), Action::Approve(c.call, true)),
                Span::new("  ", Style::PLAIN),
                Span::button("[ n  no ]", BAD.bold(), Action::Approve(c.call, false)),
            ])
        } else {
            Line::plain("  waiting for an answer from another client", MUTED)
        });
    }
    out
}

/// A call's output rows behind the card's `│` gutter.
fn output_rows(c: &Card, width: usize, verbose: bool) -> Vec<Line> {
    let bar = [Span::new("  │ ", MUTED)];
    let mut rows = Vec::new();
    let mut add = |text: &str, style: Style| {
        for line in sanitize(text).split('\n') {
            rows.extend(wrap(&[Span::new(line, style)], width, &bar, &bar));
        }
    };
    if verbose && c.input.get("command").is_none() {
        add(&c.input.to_string(), MUTED);
    }
    match c.outcome {
        Some(Outcome::Done { output }) => match output["stdout"].as_str() {
            Some(stdout) => {
                let stderr = output["stderr"].as_str().unwrap_or_default();
                for (text, style) in [(stdout, Style::PLAIN), (stderr, BAD.dim())] {
                    let text = text.strip_suffix('\n').unwrap_or(text);
                    if !text.is_empty() {
                        add(text, style);
                    }
                }
            }
            None if output.is_null() => {}
            None => {
                let text = output
                    .as_str()
                    .map_or_else(|| output.to_string(), str::to_owned);
                add(&text, Style::PLAIN);
            }
        },
        Some(Outcome::Failed { error }) => add(error, BAD),
        Some(Outcome::Interrupted) => add("the daemon stopped mid-call", WARN),
        None => {}
    }
    rows
}

/// A tool call as shown: a command as `$ command`, anything else as the
/// tool and its input (as the line client and the web UI show it).
pub fn call_text(tool: &str, input: &Value) -> String {
    match input["command"].as_str() {
        Some(command) => format!("$ {command}"),
        None => format!("{tool} {input}"),
    }
}

/// What a session did, shown after an attach's replay.
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

fn decided(decision: &Decision) -> String {
    match decision {
        Decision::PrepareStep { .. } => "request replaced".into(),
        Decision::PreTool { tool, .. } => format!("{tool} input replaced"),
        Decision::Block { tool, reason } => format!("{tool} blocked: {reason}"),
        Decision::Permission { tool, permission } => {
            format!("{tool} {}", format!("{permission:?}").to_lowercase())
        }
        Decision::Approval { tool, approved, .. } => {
            format!("{tool} {}", if *approved { "approved" } else { "denied" })
        }
        Decision::PostTool { .. } => "result replaced".into(),
        Decision::OnStop { text } => format!("continued: {}", sanitize(text)),
        #[allow(unreachable_patterns)]
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    fn tool(outcome: Option<Outcome>, approval: Option<Approval>, asked: bool) -> Item {
        Item::Tool {
            call: 7,
            tool: "run_command".into(),
            input: json!({"command": "ls"}),
            approval,
            outcome,
            asked,
        }
    }

    #[test]
    fn a_finished_card_shows_its_exit_and_output_tail() {
        let stdout = (1..=15).fold(String::new(), |s, n| s + &format!("line {n}\n"));
        let done = Outcome::Done {
            output: json!({"stdout": stdout, "stderr": "", "exit": 0}),
        };
        let rows = texts(&rows(
            &tool(Some(done.clone()), None, false),
            40,
            Look::default(),
        ));
        assert_eq!(rows[1], "▾ $ ls  ✓ exit 0");
        assert_eq!(rows[2], "  │ ⋯ 5 earlier lines (ctrl-o shows all)");
        assert_eq!(rows.last().unwrap(), "  │ line 15");
        let all = rows_of(
            &tool(Some(done.clone()), None, false),
            Look {
                verbose: true,
                ..Look::default()
            },
        );
        assert_eq!(all.len(), 2 + 15);
        let folded = rows_of(
            &tool(Some(done), None, false),
            Look {
                collapsed: true,
                ..Look::default()
            },
        );
        assert_eq!(folded[1..], ["▸ $ ls  ✓ exit 0", "  │ 15 lines"]);
    }

    fn rows_of(item: &Item, look: Look) -> Vec<String> {
        texts(&rows(item, 40, look))
    }

    #[test]
    fn a_card_asked_of_this_client_has_buttons() {
        let item = tool(None, Some(Approval::Pending), true);
        let lines = rows(&item, 60, Look::default());
        assert_eq!(texts(&lines)[1], "• $ ls  ? waiting for approval");
        let buttons: Vec<_> = lines[2].spans.iter().filter_map(|s| s.action).collect();
        assert_eq!(
            buttons,
            [Action::Approve(7, true), Action::Approve(7, false)]
        );
        let elsewhere = rows_of(&tool(None, Some(Approval::Pending), false), Look::default());
        assert_eq!(elsewhere[2], "  waiting for an answer from another client");
    }

    #[test]
    fn a_streaming_reply_shows_its_live_text() {
        let item = Item::Reply {
            call: 3,
            model: "echo".into(),
            text: String::new(),
            status: Status::Running,
            usage: None,
        };
        let look = Look {
            streaming: Some("hello **world**"),
            ..Look::default()
        };
        assert_eq!(rows_of(&item, look)[1], "  hello world▍");
        assert_eq!(rows_of(&item, Look::default())[1], "  … echo");
    }

    #[test]
    fn command_output_escapes_are_stripped() {
        let done = Outcome::Done {
            output: json!({"stdout": "\x1b[31mred\x1b[0m\n", "stderr": "", "exit": 1}),
        };
        let rows = rows_of(&tool(Some(done), None, false), Look::default());
        assert_eq!(rows[1..], ["▾ $ ls  ✗ exit 1", "  │ red"]);
    }
}
