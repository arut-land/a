//! Transcript items as rows: messages, replies, and tool call cards.

use protocol::view::{Approval, Item, Status, call_title, output_text};
use protocol::{Decision, Outcome};
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
        Item::Tool { outcome, .. } | Item::Command { outcome, .. } => outcome.is_some(),
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
            card: shown,
            asked,
        } => card(
            &Card {
                call: *call,
                title: call_title(tool, input, shown.as_ref()),
                input: input.get("command").is_none().then_some(input),
                approval: *approval,
                outcome: outcome.as_ref(),
                shown: shown.as_ref(),
                asked: *asked,
            },
            width,
            look,
        ),
        Item::Command {
            call,
            name,
            args,
            outcome,
        } => card(
            &Card {
                call: *call,
                title: format!("/{name} {args}").trim_end().to_owned(),
                input: None,
                approval: None,
                outcome: outcome.as_ref(),
                shown: None,
                asked: false,
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

/// A tool call or a slash command, as a card.
struct Card<'a> {
    call: u64,
    title: String,
    /// Input the title does not show, for verbose mode.
    input: Option<&'a Value>,
    approval: Option<Approval>,
    outcome: Option<&'a Outcome>,
    /// How the tool presents it.
    shown: Option<&'a protocol::Card>,
    asked: bool,
}

/// A tool call: a header with its state (a click folds it), its output's
/// tail, and while it waits for this client, the approval buttons.
fn card(c: &Card, width: usize, look: Look) -> Vec<Line> {
    let toggle = Action::Toggle(c.call);
    let (status, style) = match (c.outcome, c.approval) {
        (Some(Outcome::Done { output }), _) => {
            let note = output_text(output, c.shown)
                .map(|(_, note)| note)
                .filter(|note| !note.is_empty())
                .unwrap_or_else(|| "done".to_owned());
            if failed(output) {
                (format!("✗ {note}"), BAD)
            } else {
                (format!("✓ {note}"), OK)
            }
        }
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
    let room = width.saturating_sub(2 + 2 + super::text::width_of(&status));
    let mut out = vec![
        Line::default(),
        Line::from(vec![
            Span::button(marker, MUTED, toggle),
            Span::button(
                ellipsize(&sanitize(&c.title).replace('\n', " "), room),
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

/// Whether a finished command did not succeed: a nonzero exit, a signal,
/// a timeout or an interrupt.
fn failed(output: &Value) -> bool {
    output["exit"].as_i64().is_some_and(|code| code != 0)
        || !output["signal"].is_null()
        || !output["timed_out"].is_null()
        || output["aborted"] == true
}

/// A call's output rows behind the card's `│` gutter; a diff colored as one.
fn output_rows(c: &Card, width: usize, verbose: bool) -> Vec<Line> {
    let bar = [Span::new("  │ ", MUTED)];
    let mut rows = Vec::new();
    let mut add = |text: &str, style: Style, diff: bool| {
        for line in sanitize(text).split('\n') {
            let style = match line.as_bytes().first() {
                Some(b'@') if diff => Style::fg(Color::Cyan),
                Some(b'+') if diff => OK,
                Some(b'-') if diff => BAD,
                _ => style,
            };
            rows.extend(wrap(&[Span::new(line, style)], width, &bar, &bar));
        }
    };
    if verbose && let Some(input) = c.input {
        add(&input.to_string(), MUTED, false);
    }
    match c.outcome {
        Some(Outcome::Done { output }) => match output_text(output, c.shown) {
            Some((text, _)) => {
                let text = text.strip_suffix('\n').unwrap_or(&text);
                if !text.is_empty() {
                    add(text, Style::PLAIN, text.starts_with("@@ "));
                }
            }
            None if output.is_null() => {}
            None => {
                let text = output
                    .as_str()
                    .map_or_else(|| output.to_string(), str::to_owned);
                add(&text, Style::PLAIN, false);
            }
        },
        Some(Outcome::Failed { error }) => add(error, BAD, false),
        Some(Outcome::Interrupted) => add("the daemon stopped mid-call", WARN, false),
        None => {}
    }
    rows
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
            tool: "bash".into(),
            input: json!({"command": "ls"}),
            approval,
            outcome,
            card: None,
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

    #[test]
    fn an_edit_card_shows_its_diff() {
        let item = Item::Tool {
            call: 7,
            tool: "edit".into(),
            input: json!({"path": "a.txt", "edits": []}),
            approval: None,
            outcome: Some(Outcome::Done {
                output: json!({"path": "a.txt"}),
            }),
            card: Some(protocol::Card {
                title: "edit a.txt".into(),
                body: "@@ -2,1 +2,1 @@\n-two\n+2\n".into(),
                note: "+1 -1".into(),
            }),
            asked: false,
        };
        let lines = rows(&item, 40, Look::default());
        assert_eq!(
            texts(&lines)[1..],
            [
                "▾ edit a.txt  ✓ +1 -1",
                "  │ @@ -2,1 +2,1 @@",
                "  │ -two",
                "  │ +2"
            ]
        );
        let color = |row: usize| lines[row].spans[1].style;
        assert_eq!((color(3), color(4)), (BAD, OK));
    }

    #[test]
    fn a_slash_command_shows_as_a_card() {
        let mut item = Item::Command {
            call: 4,
            name: "model".into(),
            args: String::new(),
            outcome: None,
        };
        assert!(!settled(&item));
        assert_eq!(rows_of(&item, Look::default())[1], "• /model  running…");
        if let Item::Command { outcome, .. } = &mut item {
            *outcome = Some(Outcome::Failed {
                error: "no command /model".into(),
            });
        }
        assert_eq!(
            rows_of(&item, Look::default())[1..],
            ["▾ /model  ✗ failed", "  │ no command /model"]
        );
    }
}
