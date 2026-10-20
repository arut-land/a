//! Transcript items as rows: messages, replies, and tool call cards, laid
//! out from what [`Item::present`] says of each.

use protocol::view::md::{self, Block, Inline};
use protocol::view::{Buttons, Glyph, Item, Kind, Presented, Shown};
use protocol::{CallId, Json};

use ratatui_core::style::{Color, Modifier, Style};

use super::render::ellipsize;
use super::text::{Action, Line, Span, sanitize, wrap};

/// How much of a card's output shows unless everything is expanded.
const TAIL: usize = 10;

pub const MUTED: Style = Style::new().fg(Color::DarkGray);
pub const USER: Style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
pub const CODE: Style = Style::new().fg(Color::Cyan);
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

/// The colour of a status class.
const fn style(glyph: Glyph) -> Style {
    match glyph {
        Glyph::None => Style::new(),
        Glyph::Ok => OK,
        Glyph::Bad | Glyph::Denied => BAD,
        Glyph::Warn => WARN,
        Glyph::Busy => BUSY,
        Glyph::Ask => ASK,
    }
}

/// `✓ exit 0`: the sign and the status, as one span.
fn status(p: &Presented<'_>) -> Span {
    let sign = p.glyph.sign();
    let text = match (sign.is_empty(), p.status.is_empty()) {
        (true, _) => p.status.to_string(),
        (false, true) => sign.to_owned(),
        (false, false) => format!("{sign} {}", p.status),
    };
    Span::new(text, style(p.glyph))
}

pub fn rows(item: &Item, width: usize, look: Look) -> Vec<Line> {
    let blank = Line::default();
    let p = item.present();
    match p.kind {
        Kind::User => {
            let mut out = vec![blank];
            for (n, line) in sanitize(&p.body.text()).split('\n').enumerate() {
                let first = Span::new(if n == 0 { "› " } else { "  " }, USER);
                let body = [Span::new(line, Style::new().bold())];
                out.extend(wrap(&body, width, &[first], &[Span::new("  ", USER)]));
            }
            out
        }
        Kind::Assistant => {
            let mut out = vec![blank];
            out.extend(markdown(&p.body.text(), width, Style::new()));
            out
        }
        Kind::Reply => reply(&p, width),
        // A side request's reply is its extension's: one muted line.
        Kind::Side => {
            let text = p.body.text();
            let line = text.lines().next().unwrap_or_default();
            vec![Line::plain(
                ellipsize(&sanitize(&format!("  · {}: {line}", p.title)), width),
                MUTED,
            )]
        }
        Kind::Tool | Kind::Command => {
            let input = match item {
                Item::Tool { input, .. } if p.kind == Kind::Tool => {
                    protocol::view::command(input).is_none().then_some(input)
                }
                _ => None,
            };
            card(item.key(), &p, input, width, look)
        }
        Kind::Hook | Kind::Environment => vec![Line::plain(
            ellipsize(&sanitize(&format!("  · {}", p.title)), width),
            MUTED,
        )],
        Kind::Child => {
            let head = vec![
                Span::new("⑂ ", BUSY),
                Span::new(format!("{} ", p.title), Style::new().bold()),
                status(&p),
            ];
            let mut out = vec![blank, Line::from(head)];
            let body = [Span::new(sanitize(&p.body.text()).replace('\n', " "), MUTED)];
            let pad = [Span::new("  ", MUTED)];
            out.extend(wrap(&body, width, &pad, &pad).into_iter().take(3));
            out
        }
    }
}

fn reply(p: &Presented<'_>, width: usize) -> Vec<Line> {
    let mut out = vec![Line::default()];
    match (p.glyph, &p.body) {
        (Glyph::Busy, _) => out.push(Line::plain(format!("  … {}", p.title), MUTED)),
        (Glyph::None, Shown::Markdown("")) => return Vec::new(),
        (Glyph::None, body) => out.extend(markdown(&body.text(), width, Style::new())),
        (glyph, _) => {
            let body = [Span::new(
                format!("{} {}", glyph.sign(), sanitize(&p.status)),
                style(glyph),
            )];
            out.extend(wrap(
                &body,
                width,
                &[Span::new("  ", style(glyph))],
                &[Span::new("    ", style(glyph))],
            ));
        }
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
            self.fenced = markdown_into(rows, &ended, width, Style::new(), self.fenced);
            (self.done, self.kept) = (end + 1, rows.len());
        }
        let open = sanitize(&text[self.done..]);
        markdown_into(rows, &open, width, Style::new(), self.fenced);
        if let Some(last) = rows.last_mut() {
            last.spans.push(Span::new("▍", MUTED));
        }
        self.seen = text.len();
    }
}

/// A reply's markdown as rows: `#` headings bold, `-`/`*` bullets as `•`,
/// `>` quotes, fenced code verbatim, and `**bold**` and `` `code` `` inline.
pub fn markdown(text: &str, width: usize, base: Style) -> Vec<Line> {
    let mut out = Vec::new();
    markdown_into(&mut out, &sanitize(text), width, base, false);
    out
}

/// [`markdown`] of sanitized `text`, appended to `out`; `fenced` says
/// whether it starts inside a code fence, and whether it ends in one comes
/// back.
fn markdown_into(out: &mut Vec<Line>, text: &str, width: usize, base: Style, fenced: bool) -> bool {
    let pad = |s: String, style| vec![Span::new(s, style)];
    let mut blocks = md::blocks(text).fenced(fenced);
    for block in &mut blocks {
        let (first, rest, body, style) = match block {
            Block::Fence(line) => {
                out.push(Line::plain(format!("  {line}"), CODE.dim()));
                continue;
            }
            // Code is not reflowed; long lines are cut by the screen.
            Block::Code(line) => {
                out.push(Line::plain(format!("  {line}"), CODE));
                continue;
            }
            Block::Heading(_, text) => (pad("  ".into(), base), pad("  ".into(), base), text, base.bold()),
            Block::Bullet { indent, text } => {
                let pad_to = " ".repeat(usize::from(indent));
                (
                    pad(format!("  {pad_to}• "), base),
                    pad(format!("  {pad_to}  "), base),
                    text,
                    base,
                )
            }
            Block::Quote(text) => (
                pad("  │ ".into(), base.dim()),
                pad("  │ ".into(), base.dim()),
                text,
                base.italic(),
            ),
            Block::Para(text) => (pad("  ".into(), base), pad("  ".into(), base), text, base),
        };
        out.extend(wrap(&inline(body, style), width, &first, &rest));
    }
    blocks.in_fence()
}

/// Inline `**bold**` and `` `code` `` within one line.
fn inline(text: &str, base: Style) -> Vec<Span> {
    md::inlines(text)
        .map(|run| match run {
            Inline::Text(text) => Span::new(text, base),
            Inline::Bold(text) => Span::new(text, base.bold()),
            Inline::Code(text) => Span::new(text, CODE),
        })
        .collect()
}

/// A tool call or a slash command, as a card: a header with its state (a
/// click folds it), its output's tail, and while it waits for this
/// client, the approval buttons. `input` is what the title does not show,
/// for verbose mode.
fn card(key: protocol::Seq, p: &Presented<'_>, input: Option<&Json>, width: usize, look: Look) -> Vec<Line> {
    let toggle = Action::Toggle(key);
    let status = status(p);
    let body = output_rows(p, input, width, look.verbose);
    let marker = match (body.is_empty(), look.collapsed) {
        (true, _) => "• ",
        (false, true) => "▸ ",
        (false, false) => "▾ ",
    };
    let room = width.saturating_sub(2 + 2 + super::text::width_of(&status.text));
    let mut out = vec![
        Line::default(),
        Line::from(vec![
            Span::button(marker, MUTED, toggle),
            Span::button(
                ellipsize(&sanitize(&p.title).replace('\n', " "), room),
                Style::new().bold(),
                toggle,
            ),
            Span::button("  ", Style::new(), toggle),
            Span::button(status.text, status.style, toggle),
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
    if let Buttons::Approve(call) = p.buttons {
        out.push(approve_row(call));
    }
    out
}

/// The row that answers call `call`'s ask.
fn approve_row(call: CallId) -> Line {
    Line::from(vec![
        Span::new("  allow this call?  ", ASK),
        Span::button("[ y  yes ]", OK.bold(), Action::Approve(call, true)),
        Span::new("  ", Style::new()),
        Span::button("[ n  no ]", BAD.bold(), Action::Approve(call, false)),
    ])
}

/// A call's output rows behind the card's `│` gutter; a diff colored as one.
fn output_rows(p: &Presented<'_>, input: Option<&Json>, width: usize, verbose: bool) -> Vec<Line> {
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
    if verbose && let Some(input) = input {
        add(&input.to_string(), MUTED, false);
    }
    match &p.body {
        Shown::None => {}
        Shown::Diff(text) => add(text, Style::new(), true),
        body => add(&body.text(), Style::new(), false),
    }
    rows
}

#[cfg(test)]
mod tests {
    use protocol::ledger::Phase;
    use protocol::{Audience, Outcome, WaitOn};
    use serde_json::json;

    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    fn tool(outcome: Option<Outcome>, phase: Phase) -> Item {
        Item::Tool {
            call: CallId::new(7).unwrap(),
            tool: "bash".into(),
            input: Json::encode(&json!({"command": "ls"})).unwrap(),
            phase,
            outcome,
            card: None,
            by: Audience::Model,
        }
    }

    #[test]
    fn a_finished_card_shows_its_exit_and_output_tail() {
        let stdout = (1..=15).fold(String::new(), |s, n| s + &format!("line {n}\n"));
        let done = Outcome::Done {
            output: Json::encode(&json!({"output": stdout, "exit": 0})).unwrap(),
        };
        let mut finished = tool(Some(done.clone()), Phase::Running);
        if let Item::Tool { card, .. } = &mut finished {
            *card = Some(protocol::Card {
                title: "$ ls".into(),
                body: stdout.clone(),
                note: "exit 0".into(),
                ..protocol::Card::default()
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
        let item = tool(None, Phase::Waiting(WaitOn::Approval));
        let lines = rows(&item, 60, Look::default());
        assert_eq!(texts(&lines)[1], "• $ ls  ? waiting for approval");
        let buttons: Vec<_> = lines[2].spans.iter().filter_map(|s| s.action).collect();
        let call = CallId::new(7).unwrap();
        assert_eq!(
            buttons,
            [Action::Approve(call, true), Action::Approve(call, false)]
        );
    }

    #[test]
    fn a_streaming_reply_shows_its_live_text() {
        let item = Item::Reply {
            call: CallId::new(3).unwrap(),
            model: "echo".into(),
            text: String::new(),
            status: protocol::view::Status::Running,
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
    fn markdown_rows_are_headed_bulleted_and_fenced() {
        let rows = markdown("# Title\n- one **two** `x`\n```\nfn a()\n```", 40, Style::new());
        assert_eq!(
            texts(&rows),
            ["  Title", "  • one two x", "  ```", "  fn a()", "  ```"]
        );
        assert!(rows[0].spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(rows[1].spans[3].style, CODE);
    }

    /// The shell marks a failed command's card: no client reads its exit
    /// code out of the output.
    #[test]
    fn command_output_escapes_are_stripped_and_a_bad_mark_shows_red() {
        let done = Outcome::Done {
            output: Json::encode(&json!({"output": "\x1b[31mred\x1b[0m\n", "exit": 1})).unwrap(),
        };
        let mut ran = tool(Some(done), Phase::Running);
        if let Item::Tool { card, .. } = &mut ran {
            *card = Some(protocol::Card {
                title: "$ ls".into(),
                body: "\x1b[31mred\x1b[0m\n".into(),
                note: "exit 1".into(),
                mark: protocol::Mark::Bad,
            });
        }
        let lines = rows(&ran, 40, Look::default());
        assert_eq!(texts(&lines)[1..], ["▾ $ ls  ✗ exit 1", "  │ red"]);
        assert_eq!(lines[1].spans[3].style, BAD);
    }

    #[test]
    fn an_edit_card_shows_its_diff() {
        let item = Item::Tool {
            call: CallId::new(7).unwrap(),
            tool: "edit".into(),
            input: Json::encode(&json!({"path": "a.txt", "edits": []})).unwrap(),
            phase: Phase::Running,
            outcome: Some(Outcome::Done {
                output: Json::encode(&json!({"path": "a.txt"})).unwrap(),
            }),
            card: Some(protocol::Card {
                title: "edit a.txt".into(),
                body: "@@ -2,1 +2,1 @@\n-two\n+2\n".into(),
                note: "+1 -1".into(),
                ..protocol::Card::default()
            }),
            by: Audience::Model,
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
        let mut item = Item::Tool {
            call: CallId::new(4).unwrap(),
            tool: "model".into(),
            input: Json::encode(&serde_json::json!({ "args": "" })).unwrap(),
            phase: Phase::Gating { at: 0 },
            outcome: None,
            card: None,
            by: Audience::User,
        };
        assert!(!item.settled());
        assert_eq!(rows_of(&item, Look::default())[1], "• /model  running…");
        if let Item::Tool { outcome, .. } = &mut item {
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
