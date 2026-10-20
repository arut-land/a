//! Transcript items as rows: messages, replies, and tool call cards.

use protocol::view::{Item, Phase, Status, call_title, decision_line, output_text};
use protocol::{Json, Outcome};
use serde_json::Value;

use ratatui_core::style::{Color, Modifier, Style};

use super::render::ellipsize;
use super::text::{Action, Line, Span, markdown, markdown_into, sanitize, wrap};

/// How much of a card's output shows unless everything is expanded.
const TAIL: usize = 10;

pub const MUTED: Style = Style::new().fg(Color::DarkGray);
pub const USER: Style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
const OK: Style = Style::new().fg(Color::Green);
const BAD: Style = Style::new().fg(Color::Red);
const WARN: Style = Style::new().fg(Color::Yellow);
const BUSY: Style = Style::new().fg(Color::Blue);
pub const ASK: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

/// How an item is shown.
#[derive(Clone, Copy, Debug, Default)]
pub struct Look {
    /// The card is folded to its header.
    pub collapsed: bool,
    /// Every card's whole output and input (Ctrl-O).
    pub verbose: bool,
}

/// Whether an item will not change any more: an inline viewport prints
/// those into the scrollback.
pub fn settled(item: &Item) -> bool {
    item.phase().is_none_or(Phase::settled)
}

pub fn rows(item: &Item, width: usize, look: Look) -> Vec<Line> {
    let blank = Line::default();
    match item {
        Item::User { text, .. } => {
            let mut out = vec![blank];
            for (n, line) in sanitize(text).split('\n').enumerate() {
                let first = Span::new(if n == 0 { "› " } else { "  " }, USER);
                let body = [Span::new(line, Style::new().bold())];
                out.extend(wrap(&body, width, &[first], &[Span::new("  ", USER)]));
            }
            out
        }
        Item::Assistant { text, .. } => {
            let mut out = vec![blank];
            out.extend(markdown(text, width, Style::new()));
            out
        }
        Item::Reply {
            model,
            text,
            status,
            ..
        } => reply(model, text, status, width),
        Item::Tool {
            call,
            tool,
            input,
            outcome,
            card: shown,
            ..
        } => card(
            &Card {
                call: *call,
                title: call_title(tool, input, shown.as_ref()),
                input: input.value().get("command").is_none().then_some(input),
                phase: item.phase().unwrap_or(Phase::Running),
                outcome: outcome.as_ref(),
                shown: shown.as_ref(),
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
                phase: item.phase().unwrap_or(Phase::Running),
                outcome: outcome.as_ref(),
                shown: None,
            },
            width,
            look,
        ),
        Item::Hook {
            hook,
            call,
            tool,
            decision,
            ..
        } => {
            let line = decision_line(tool, *call, decision);
            vec![Line::plain(
                ellipsize(&sanitize(&format!("  · hook {hook}: {line}")), width),
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
                Span::new(format!("child {} ", child.short()), Style::new().bold()),
            ];
            head.push(match fired {
                None => Span::new("running…", BUSY),
                Some(Outcome::Done { .. }) => Span::new("✓ done", OK),
                Some(Outcome::Failed { error }) => Span::new(format!("✗ {error}"), BAD),
                Some(Outcome::Interrupted { .. }) => Span::new("⚠ interrupted", WARN),
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

fn reply(model: &str, text: &str, status: &Status, width: usize) -> Vec<Line> {
    let mut out = vec![Line::default()];
    match status {
        Status::Running => out.push(Line::plain(format!("  … {model}"), MUTED)),
        Status::Done if text.is_empty() => return Vec::new(),
        Status::Done => out.extend(markdown(text, width, Style::new())),
        Status::Failed { error } => {
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
        Status::Interrupted => out.push(Line::plain("  ⚠ model call interrupted", WARN)),
    }
    out
}

/// Where a reply streaming in was wrapped to: the lines that ended are
/// wrapped once and kept; each delta wraps only the open last line again,
/// so a long reply costs no more per delta than a short one.
#[derive(Clone, Copy, Debug, Default)]
pub struct Live {
    /// The text's bytes wrapped for good (up to a newline), whether they
    /// end inside a code fence, and the rows they made.
    done: usize,
    fenced: bool,
    kept: usize,
    /// The text's length when last wrapped.
    seen: usize,
}

impl Live {
    /// Brings `rows` (empty, or as this left them) up to `text`, the
    /// reply's live text so far.
    pub fn update(&mut self, rows: &mut Vec<Line>, text: &str, width: usize) {
        if rows.is_empty() || text.len() < self.seen {
            *self = Self::default();
            rows.clear();
            rows.push(Line::default());
            self.kept = 1;
        } else if text.len() == self.seen {
            return;
        }
        rows.truncate(self.kept);
        if let Some(nl) = text[self.done..].rfind('\n') {
            let end = self.done + nl;
            let ended = sanitize(&text[self.done..end]);
            markdown_into(rows, &ended, width, Style::new(), &mut self.fenced);
            (self.done, self.kept) = (end + 1, rows.len());
        }
        let open = sanitize(&text[self.done..]);
        markdown_into(rows, &open, width, Style::new(), &mut self.fenced.clone());
        if let Some(last) = rows.last_mut() {
            last.spans.push(Span::new("▍", MUTED));
        }
        self.seen = text.len();
    }
}

/// A tool call or a slash command, as a card.
struct Card<'a> {
    call: u64,
    title: String,
    /// Input the title does not show, for verbose mode.
    input: Option<&'a Json>,
    phase: Phase,
    outcome: Option<&'a Outcome>,
    /// How the tool presents it.
    shown: Option<&'a protocol::Card>,
}

/// A tool call: a header with its state (a click folds it), its output's
/// tail, and while it waits for this client, the approval buttons.
fn card(c: &Card, width: usize, look: Look) -> Vec<Line> {
    let toggle = Action::Toggle(c.call);
    let (status, style) = match (c.outcome, c.phase) {
        (Some(Outcome::Done { output }), _) => {
            let note = output_text(c.shown)
                .map(|(_, note)| note)
                .filter(|note| !note.is_empty())
                .unwrap_or_else(|| "done".to_owned());
            if failed(&output.value()) {
                (format!("✗ {note}"), BAD)
            } else {
                (format!("✓ {note}"), OK)
            }
        }
        (Some(Outcome::Failed { .. }), _) => ("✗ failed".to_owned(), BAD),
        (Some(Outcome::Interrupted { .. }), _) => ("⚠ interrupted".to_owned(), WARN),
        (None, Phase::Asking) => ("? waiting for approval".to_owned(), ASK),
        (None, Phase::Denied) => ("⊘ not approved".to_owned(), BAD),
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
                Style::new().bold(),
                toggle,
            ),
            Span::button("  ", Style::new(), toggle),
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
    if c.phase == Phase::Asking {
        out.push(Line::from(vec![
            Span::new("  allow this call?  ", ASK),
            Span::button("[ y  yes ]", OK.bold(), Action::Approve(c.call, true)),
            Span::new("  ", Style::new()),
            Span::button("[ n  no ]", BAD.bold(), Action::Approve(c.call, false)),
        ]));
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
                Some(b'@') if diff => Style::new().fg(Color::Cyan),
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
        Some(Outcome::Done { output }) => match output_text(c.shown) {
            Some((text, _)) => {
                let text = text.strip_suffix('\n').unwrap_or(&text);
                if !text.is_empty() {
                    add(text, Style::new(), text.starts_with("@@ "));
                }
            }
            None => match output.value() {
                Value::Null => {}
                output => {
                    let text = output
                        .as_str()
                        .map_or_else(|| output.to_string(), str::to_owned);
                    add(&text, Style::new(), false);
                }
            },
        },
        Some(Outcome::Failed { error }) => add(error, BAD, false),
        Some(Outcome::Interrupted { output }) => {
            if let Some(text) = output.as_deref().map(|t| t.trim_end_matches('\n')) {
                add(text, Style::new(), false);
            }
            add("stopped before it finished", WARN, false);
        }
        None => {}
    }
    rows
}

#[cfg(test)]
mod tests {
    use protocol::view::Approval;
    use serde_json::json;

    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    fn tool(outcome: Option<Outcome>, approval: Option<Approval>) -> Item {
        Item::Tool {
            call: 7,
            tool: "bash".into(),
            input: json!({"command": "ls"}).into(),
            approval,
            outcome,
            card: None,
        }
    }

    #[test]
    fn a_finished_card_shows_its_exit_and_output_tail() {
        let stdout = (1..=15).fold(String::new(), |s, n| s + &format!("line {n}\n"));
        let done = Outcome::Done {
            output: json!({"output": stdout, "exit": 0}).into(),
        };
        let mut finished = tool(Some(done.clone()), None);
        if let Item::Tool { card, .. } = &mut finished {
            *card = Some(protocol::Card {
                title: "$ ls".into(),
                body: stdout.clone(),
                note: "exit 0".into(),
            });
        }
        let rows = texts(&rows(&finished, 40, Look::default()));
        assert_eq!(rows[1], "▾ $ ls  ✓ exit 0");
        assert_eq!(rows[2], "  │ ⋯ 5 earlier lines (ctrl-o shows all)");
        assert_eq!(rows.last().unwrap(), "  │ line 15");
        let all = rows_of(
            &finished,
            Look {
                verbose: true,
                ..Look::default()
            },
        );
        assert_eq!(all.len(), 2 + 15);
        let folded = rows_of(
            &finished,
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
    fn a_card_waiting_for_approval_has_buttons() {
        let item = tool(None, Some(Approval::Pending));
        let lines = rows(&item, 60, Look::default());
        assert_eq!(texts(&lines)[1], "• $ ls  ? waiting for approval");
        let buttons: Vec<_> = lines[2].spans.iter().filter_map(|s| s.action).collect();
        assert_eq!(
            buttons,
            [Action::Approve(7, true), Action::Approve(7, false)]
        );
    }

    #[test]
    fn a_streaming_reply_shows_its_live_text() {
        let item = Item::Reply {
            call: 3,
            model: "echo".into(),
            text: String::new(),
            status: Status::Running,
            usage: None,
            purpose: None,
        };
        assert_eq!(rows_of(&item, Look::default())[1], "  … echo");
        let mut live = Live::default();
        let mut lines = Vec::new();
        live.update(&mut lines, "hello **world**", 40);
        assert_eq!(texts(&lines)[1], "  hello world▍");
    }

    #[test]
    fn a_streaming_reply_wraps_as_its_whole_text_would() {
        let text =
            "# Plan\n\nsome **bold** words that wrap past the width\n```\nfn a() {}\n```\n- done";
        let mut live = Live::default();
        let mut lines = Vec::new();
        for end in (0..=text.len()).filter(|&i| text.is_char_boundary(i)) {
            live.update(&mut lines, &text[..end], 20);
            let mut whole = vec![Line::default()];
            whole.extend(markdown(&text[..end], 20, Style::new()));
            whole.last_mut().unwrap().spans.push(Span::new("▍", MUTED));
            assert_eq!(lines, whole, "at {end}");
        }
    }

    #[test]
    fn command_output_escapes_are_stripped() {
        let done = Outcome::Done {
            output: json!({"output": "\x1b[31mred\x1b[0m\n", "exit": 1}).into(),
        };
        let mut ran = tool(Some(done), None);
        if let Item::Tool { card, .. } = &mut ran {
            *card = Some(protocol::Card {
                title: "$ ls".into(),
                body: "\x1b[31mred\x1b[0m\n".into(),
                note: "exit 1".into(),
            });
        }
        let rows = rows_of(&ran, Look::default());
        assert_eq!(rows[1..], ["▾ $ ls  ✗ exit 1", "  │ red"]);
    }

    #[test]
    fn an_edit_card_shows_its_diff() {
        let item = Item::Tool {
            call: 7,
            tool: "edit".into(),
            input: json!({"path": "a.txt", "edits": []}).into(),
            approval: None,
            outcome: Some(Outcome::Done {
                output: json!({"path": "a.txt"}).into(),
            }),
            card: Some(protocol::Card {
                title: "edit a.txt".into(),
                body: "@@ -2,1 +2,1 @@\n-two\n+2\n".into(),
                note: "+1 -1".into(),
            }),
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
