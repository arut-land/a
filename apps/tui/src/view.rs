//! Transcript items as rows: messages, replies, and tool call cards, laid
//! out from what [`Item::present`] says of each.

use std::borrow::Cow;

use protocol::view::md::{self, Block, Inline};
use protocol::view::{Glyph, Item, Kind, Presented, Shown};
use protocol::{CallId, Json};

use super::render::ellipsize;
use super::text::{Action, Color, Line, Modifier, Span, Style, sanitize, wrap, wrap_checkpoints};

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
    pub deny: bool,
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
    presented_rows(item, &item.present(), width, look)
}

pub fn presented_rows(item: &Item, p: &Presented<'_>, width: usize, look: Look) -> Vec<Line> {
    let blank = Line::default();
    match p.kind {
        Kind::User => {
            let mut out = vec![blank];
            for (n, line) in sanitize(&p.body.text()).split('\n').enumerate() {
                let first = Span::new(if n == 0 { "❯ " } else { "  " }, USER);
                let body = [Span::new(line, Style::new().bold())];
                out.extend(wrap(&body, width, &[first], &[Span::new("  ", USER)]));
            }
            out
        }
        Kind::Assistant => {
            let mut out = vec![blank];
            out.extend(markdown(&p.body.text(), width, Style::new()));
            mark_reply(&mut out);
            out
        }
        Kind::Reply => reply(p, width),
        // A side request's reply is its extension's, not the conversation's:
        // a title shows in the status line, a summary in what the model sees.
        Kind::Side => Vec::new(),
        Kind::Tool | Kind::Command => {
            let input = match item {
                Item::Tool { input, .. } if p.kind == Kind::Tool => {
                    protocol::view::command(input).is_none().then_some(input)
                }
                _ => None,
            };
            card(item.key(), p, input, width, look)
        }
        Kind::Hook | Kind::Environment => vec![Line::plain(
            ellipsize(&sanitize(&format!("  · {}", p.title)), width),
            MUTED,
        )],
        Kind::Child => {
            let head = vec![
                Span::new("⑂ ", BUSY),
                Span::new(format!("{} ", p.title), Style::new().bold()),
                status(p),
            ];
            let mut out = vec![blank, Line::from(head)];
            let body = [Span::new(
                sanitize(&p.body.text()).replace('\n', " "),
                MUTED,
            )];
            let pad = [Span::new("  ", MUTED)];
            out.extend(wrap(&body, width, &pad, &pad).into_iter().take(3));
            out
        }
    }
}

fn reply(p: &Presented<'_>, width: usize) -> Vec<Line> {
    let mut out = vec![Line::default()];
    match (p.glyph, &p.body) {
        // The indicator under the transcript says a call is out.
        (Glyph::Busy, _) | (Glyph::None, Shown::Markdown("")) => return Vec::new(),
        (Glyph::None, body) => {
            out.extend(markdown(&body.text(), width, Style::new()));
            mark_reply(&mut out);
        }
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

/// A reply's first row (after the blank one) led by `⏺`, as Claude Code
/// marks the model's turn; the rows under it keep the two-column indent.
pub fn mark_reply(rows: &mut [Line]) {
    if let Some(first) = rows.get_mut(1).and_then(|row| row.spans.first_mut())
        && first.text.as_str() == "  "
    {
        *first = Span::new("⏺ ", Style::new());
    }
}

/// Completed display rows stay in the caller's cache. Inline state and
/// pending spaces advance once; only the unfinished visible row rewraps.
#[derive(Default)]
pub struct Live {
    identity: Option<(CallId, u16, usize)>,
    seen: usize,
    kept: usize,
    fenced: bool,
    sanitizer: Option<client::Sanitizer>,
    open: String,
    leading: usize,
    continuation: Option<Continuation>,
    #[cfg(test)]
    rewrapped: usize,
    #[cfg(test)]
    parsed: usize,
}

#[derive(Default)]
struct Continuation {
    first: Vec<Span>,
    rest: Vec<Span>,
    style: Style,
    spans: Vec<Span>,
    spaces: Vec<Span>,
    space_count: usize,
    bold: bool,
    code: bool,
    star: bool,
    leading: Leading,
}

#[derive(Default)]
enum Leading {
    #[default]
    Keep,
    Trim,
}

impl Continuation {
    fn push(&mut self, text: &str, width: usize) {
        let text = if matches!(self.leading, Leading::Trim) {
            let text = text.trim_start();
            if text.is_empty() {
                return;
            }
            self.leading = Leading::Keep;
            text
        } else {
            text
        };
        let text = if self.star {
            Cow::Owned(format!("*{text}"))
        } else {
            Cow::Borrowed(text)
        };
        self.star = false;
        let (mut at, mut from) = (0, 0);
        while at < text.len() {
            let delimiter = if text.as_bytes()[at] == b'`' {
                Some(1)
            } else if !self.code && text.as_bytes()[at..].starts_with(b"**") {
                Some(2)
            } else {
                None
            };
            if let Some(delimiter) = delimiter {
                self.push_text(&text[from..at], self.pen(), width);
                if delimiter == 1 {
                    self.code = !self.code;
                } else {
                    self.bold = !self.bold;
                }
                at += delimiter;
                from = at;
                continue;
            }
            if !self.code && at + 1 == text.len() && text.as_bytes()[at] == b'*' {
                self.star = true;
                break;
            }
            at += 1;
        }
        self.push_text(&text[from..at], self.pen(), width);
    }

    fn push_text(&mut self, text: &str, style: Style, width: usize) {
        let visible = text.trim_end_matches(' ');
        if !visible.is_empty() {
            self.spans.append(&mut self.spaces);
            self.space_count = 0;
            self.spans.push(Span::new(visible, style));
        }
        // A row can show at most `width` spaces. One more carries
        // overflow into the next word's wrap decision; later spaces
        // cannot affect its placement or any visible pen.
        let spaces = text.len() - visible.len();
        let keep = spaces.min(width.saturating_add(1).saturating_sub(self.space_count));
        if keep > 0 {
            self.spaces
                .push(Span::new(&text[text.len() - keep..], style));
        }
        self.space_count += spaces;
    }

    fn pen(&self) -> Style {
        if self.code {
            CODE
        } else if self.bold {
            self.style.bold()
        } else {
            self.style
        }
    }
}

impl Live {
    /// Brings `rows` (empty, or as this left them) up to the live text.
    pub fn update(&mut self, rows: &mut Vec<Line>, stream: &protocol::view::Stream, width: usize) {
        let identity = Some((stream.call, stream.attempt, width));
        if self.identity != identity || rows.is_empty() || stream.text.len() < self.seen {
            *self = Self {
                identity,
                kept: 1,
                ..Self::default()
            };
            rows.clear();
            rows.push(Line::default());
        } else if stream.text.len() == self.seen {
            return;
        }
        rows.truncate(self.kept);
        let added = &stream.text[self.seen..];
        let added =
            if self.sanitizer.is_some() || added.chars().any(|c| c.is_control() && c != '\n') {
                Cow::Owned(
                    self.sanitizer
                        .get_or_insert_with(client::Sanitizer::default)
                        .push(added.as_bytes()),
                )
            } else {
                Cow::Borrowed(added)
            };
        let mut parts = added.split('\n').peekable();
        while let Some(part) = parts.next() {
            if let Some(c) = &mut self.continuation {
                #[cfg(test)]
                {
                    self.parsed += part.len() + usize::from(c.star);
                }
                c.push(part, width);
            } else {
                let part = if !self.fenced && self.open.is_empty() {
                    let spaces = part.bytes().take_while(|&b| b == b' ').count();
                    self.leading += spaces;
                    &part[spaces..]
                } else {
                    part
                };
                self.open.push_str(part);
            }
            if parts.peek().is_some() {
                self.fenced = if self.continuation.is_some() || self.leading > 0 {
                    self.wrap_open(rows, width)
                } else {
                    markdown_into(rows, &self.open, width, Style::new(), self.fenced)
                };
                self.kept = rows.len();
                self.open.clear();
                self.leading = 0;
                self.continuation = None;
            } else {
                self.wrap_open(rows, width);
            }
        }
        if let Some(last) = rows.last_mut() {
            last.spans.push(Span::new("▍", MUTED));
        }
        mark_reply(rows);
        self.seen = stream.text.len();
    }

    fn wrap_open(&mut self, rows: &mut Vec<Line>, width: usize) -> bool {
        if self.continuation.is_none() {
            // Block indentation is a u8; paragraph indentation can only
            // occupy one row. Keep the full count until the block is known.
            let source = if self.leading == 0 {
                Cow::Borrowed(self.open.as_str())
            } else {
                Cow::Owned(format!(
                    "{}{open}",
                    " ".repeat(self.leading.min(width.max(usize::from(u8::MAX)))),
                    open = self.open
                ))
            };
            #[cfg(test)]
            {
                self.parsed += source.len();
            }
            let block = md::blocks(&source)
                .fenced(self.fenced)
                .next()
                .expect("a logical line always has one block");
            match block {
                Block::Fence(line) => {
                    rows.push(Line::plain(format!("  {line}"), CODE.dim()));
                    return !self.fenced;
                }
                Block::Code(line) => {
                    rows.push(Line::plain(format!("  {line}"), CODE));
                    return self.fenced;
                }
                block => {
                    let trimmed = source.trim_start();
                    let ambiguous = trimmed.is_empty()
                        || matches!(trimmed, "-" | "*" | "`" | "``")
                        || (trimmed.len() <= 6 && trimmed.bytes().all(|b| b == b'#'));
                    if ambiguous {
                        markdown_into(rows, &source, width, Style::new(), self.fenced);
                        return self.fenced;
                    }
                    let (first, rest, body, style) = layout(block, Style::new());
                    let mut continuation = Continuation {
                        first,
                        rest,
                        style,
                        leading: if matches!(block, Block::Quote("")) {
                            Leading::Trim
                        } else {
                            Leading::Keep
                        },
                        ..Continuation::default()
                    };
                    continuation.push(body, width);
                    self.continuation = Some(continuation);
                }
            }
            self.open.clear();
            self.leading = 0;
        }
        let c = self
            .continuation
            .as_mut()
            .expect("the block has a wrapping layout");
        let preview = if c.star {
            let mut spans = c.spans.clone();
            spans.extend_from_slice(&c.spaces);
            spans.push(Span::new("*", c.pen()));
            Cow::Owned(spans)
        } else {
            Cow::Borrowed(c.spans.as_slice())
        };
        #[cfg(test)]
        {
            self.rewrapped += preview.iter().map(|span| span.text.len()).sum::<usize>();
        }
        let word_start = preview
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, span)| span.text.rfind(' ').map(|at| (i, at + 1)))
            .unwrap_or((0, 0));
        let mut checkpoint = None;
        rows.extend(wrap_checkpoints(
            &preview,
            width,
            &c.first,
            &c.rest,
            |candidate| {
                // The preview's final '*' may disappear when the next delta
                // completes '**', letting its word fit on the preceding row.
                if candidate.span < c.spans.len()
                    && !(c.star
                        && candidate.word_start
                        && (candidate.span, candidate.byte) >= word_start)
                {
                    checkpoint = Some(candidate);
                }
            },
        ));
        if let Some(checkpoint) = checkpoint {
            c.spans.drain(..checkpoint.span);
            c.spans[0].text = c.spans[0].text[checkpoint.byte..].into();
            c.first.clone_from(&c.rest);
            self.kept += checkpoint.rows;
        }
        self.fenced
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
    let mut blocks = md::blocks(text).fenced(fenced);
    for block in &mut blocks {
        match block {
            Block::Fence(line) => {
                out.push(Line::plain(format!("  {line}"), CODE.dim()));
                continue;
            }
            Block::Code(line) => {
                out.push(Line::plain(format!("  {line}"), CODE));
                continue;
            }
            _ => {}
        }
        let (first, rest, body, style) = layout(block, base);
        out.extend(wrap(&inline(body, style), width, &first, &rest));
    }
    blocks.in_fence()
}

fn layout(block: Block<'_>, base: Style) -> (Vec<Span>, Vec<Span>, &str, Style) {
    let pad = |s: String, style| vec![Span::new(s, style)];
    match block {
        Block::Heading(_, text) => (
            pad("  ".into(), base),
            pad("  ".into(), base),
            text,
            base.bold(),
        ),
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
        Block::Fence(_) | Block::Code(_) => unreachable!("verbatim blocks have no wrapping layout"),
    }
}

/// Inline `**bold**` and `` `code` `` within one line.
fn inline(text: &str, base: Style) -> smallvec::SmallVec<[Span; 4]> {
    md::inlines(text)
        .map(|run| match run {
            Inline::Text(text) => Span::new(text, base),
            Inline::Bold(text) => Span::new(text, base.bold()),
            Inline::Code(text) => Span::new(text, CODE),
        })
        .collect()
}

/// The rows' text, as the test that every renderer shows an item the same
/// way compares it.
#[doc(hidden)]
pub fn texts(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect())
        .collect()
}

/// A tool call or a slash command, as a card: a header with its state (a
/// click folds it), its output's tail, and while it waits for this
/// client, the approval buttons. `input` is what the title does not show,
/// for verbose mode.
fn card(
    key: protocol::Seq,
    p: &Presented<'_>,
    input: Option<&Json>,
    width: usize,
    look: Look,
) -> Vec<Line> {
    let toggle = Action::Toggle(key);
    let status = status(p);
    let review = client::approval_parts(p);
    let body = if review.is_empty() {
        output_rows(p, input, width, look.verbose)
    } else {
        Vec::new()
    };
    // The dot takes the state's color; the output hangs under it behind `⎿`.
    let marker = "⏺ ";
    let dot = match p.glyph {
        Glyph::None => MUTED,
        glyph => style(glyph),
    };
    let room = width.saturating_sub(2 + 2 + super::text::width_of(&status.text));
    let mut out = vec![
        Line::default(),
        Line::from([
            Span::button(marker, dot, toggle),
            Span::button(
                ellipsize(&sanitize(&p.title).replace('\n', " "), room),
                Style::new().bold(),
                toggle,
            ),
            Span::button("  ", Style::new(), toggle),
            Span::button(status.text, status.style, toggle),
        ]),
    ];
    let from = out.len();
    if look.collapsed {
        if !body.is_empty() {
            out.push(Line::plain(format!("     {} lines", body.len()), MUTED));
        }
    } else {
        let hidden = if look.verbose {
            0
        } else {
            body.len().saturating_sub(TAIL)
        };
        if hidden > 0 {
            out.push(Line::from([Span::button(
                format!("     … +{hidden} lines (ctrl-o to expand)"),
                MUTED,
                toggle,
            )]));
        }
        out.extend(body.into_iter().skip(hidden));
    }
    if let Some(first) = out.get_mut(from).and_then(|row| row.spans.first_mut()) {
        first.text = format!("  ⎿  {}", first.text.trim_start_matches(' ')).into();
    }
    for part in review {
        match part {
            client::ApprovalPart::Controls { call, title } => {
                out.extend(approve_rows(call, &title, look.deny, width));
            }
            part => {
                for text in part.text().unwrap_or_default().split('\n') {
                    out.extend(wrap(
                        &[Span::new(sanitize(text), ASK)],
                        width,
                        &[Span::new("     ", MUTED)],
                        &[Span::new("     ", MUTED)],
                    ));
                }
            }
        }
    }
    out
}

/// The row that answers call `call`'s ask.
fn approve_rows(call: CallId, title: &str, deny: bool, width: usize) -> Vec<Line> {
    let mut rows = vec![Line::plain(
        ellipsize(&format!("Do you want to proceed? {title}"), width),
        ASK,
    )];
    for (no, label) in [(false, "1. Yes"), (true, "2. No")] {
        let mark = if no == deny { "❯ " } else { "  " };
        rows.push(Line::from([
            Span::button(mark, ASK, Action::Approve(call, !no)),
            Span::button(label, ASK, Action::Approve(call, !no)),
        ]));
    }
    rows
}

/// A call's output rows indented under the card's header; a diff colored as one.
fn output_rows(p: &Presented<'_>, input: Option<&Json>, width: usize, verbose: bool) -> Vec<Line> {
    let bar = [Span::new("     ", MUTED)];
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

    fn tool(outcome: Option<protocol::ToolOutcome>, phase: Phase) -> Item {
        Item::Tool {
            invoke: None,
            call: CallId::new(7).unwrap(),
            tool: "bash".into(),
            input: Json::encode(&json!({"command": "ls"})).unwrap(),
            phase,
            outcome,
            reason: None,
            card: None,
            by: Audience::Model,
        }
    }

    #[test]
    fn a_finished_card_shows_its_exit_and_output_tail() {
        let stdout = (1..=15).fold(String::new(), |s, n| s + &format!("line {n}\n"));
        let done = Outcome::Done {
            output: Json::encode(&json!({"output": stdout, "exit": 0}))
                .unwrap()
                .into(),
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
        assert_eq!(rows[1], "⏺ $ ls  ✓ exit 0");
        assert_eq!(rows[2], "  ⎿  … +5 lines (ctrl-o to expand)");
        assert_eq!(rows.last().unwrap(), "     line 15");
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
        assert_eq!(folded[1..], ["⏺ $ ls  ✓ exit 0", "  ⎿  15 lines"]);
    }

    fn rows_of(item: &Item, look: Look) -> Vec<String> {
        texts(&rows(item, 40, look))
    }

    #[test]
    fn a_card_waiting_for_approval_has_buttons() {
        let item = tool(None, Phase::Waiting(WaitOn::Approval));
        let lines = rows(&item, 60, Look::default());
        assert_eq!(texts(&lines)[1], "⏺ $ ls  ? waiting for approval");
        let mut buttons: Vec<_> = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.action)
            .filter(|action| matches!(action, Action::Approve(..)))
            .collect();
        buttons.dedup();
        let call = CallId::new(7).unwrap();
        assert_eq!(
            buttons,
            [Action::Approve(call, true), Action::Approve(call, false)]
        );
    }

    #[test]
    fn an_approval_card_explains_its_rule_even_when_collapsed() {
        let mut item = tool(None, Phase::Waiting(WaitOn::Approval));
        if let Item::Tool { reason, .. } = &mut item {
            *reason = Some(Box::new(protocol::ApprovalReason::Classifier {
                reason: "needs review".into(),
            }));
        }
        let lines = texts(&rows(
            &item,
            100,
            Look {
                collapsed: true,
                ..Look::default()
            },
        ));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Why: the classifier requested approval: needs review"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Do you want to proceed?"))
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
            owner: None,
        };
        assert!(rows_of(&item, Look::default()).len() <= 1);
        let mut live = Live::default();
        let mut lines = Vec::new();
        live.update(
            &mut lines,
            &protocol::view::Stream {
                call: CallId::new(3).unwrap(),
                attempt: 0,
                text: "hello **world**".into(),
            },
            40,
        );
        assert_eq!(texts(&lines)[1], "⏺ hello world▍");
    }

    #[test]
    fn a_streaming_reply_wraps_as_its_whole_text_would() {
        let text =
            "# Plan\n\nsome **bold** words that wrap past the width\n```\nfn a() {}\n```\n- done";
        let mut live = Live::default();
        let mut lines = Vec::new();
        for end in (0..=text.len()).filter(|&i| text.is_char_boundary(i)) {
            live.update(
                &mut lines,
                &protocol::view::Stream {
                    call: CallId::new(3).unwrap(),
                    attempt: 0,
                    text: text[..end].into(),
                },
                20,
            );
            let mut whole = vec![Line::default()];
            whole.extend(markdown(&text[..end], 20, Style::new()));
            mark_reply(&mut whole);
            whole.last_mut().unwrap().spans.push(Span::new("▍", MUTED));
            assert_eq!(lines, whole, "at {end}");
        }
    }

    #[test]
    fn incremental_rows_equal_whole_markdown_for_random_chunkings() {
        let fragments = [
            "plain words ",
            "**bold words** ",
            "`code ** literal` ",
            "*",
            "**",
            "`",
            "日本語 ",
            "e\u{301} 😀 ",
            "a_very_long_unbroken_word",
            "  ",
            "\n# Heading\n",
            "\n  - bullet words ",
            "\n> quoted words ",
            "\n```rust\ncode **raw** ",
            "\n```\n",
            "\x1b[31mred\x1b[0m ",
            "\x1b]title\x07",
            "\t",
            "\n",
            "######",
            "\n      ",
        ];
        for seed in 1..=256_u64 {
            let mut random = seed;
            let mut next = || {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                random
            };
            let mut text = String::new();
            let choices = if seed % 2 == 0 {
                &fragments[..10]
            } else {
                &fragments[..]
            };
            for _ in 0..40 {
                text.push_str(choices[usize::try_from(next() % choices.len() as u64).unwrap()]);
            }
            let mut live = Live::default();
            let mut rows = Vec::new();
            let mut stream = protocol::view::Stream {
                call: CallId::new(3).unwrap(),
                attempt: 0,
                text: String::new(),
            };
            let mut width = usize::try_from(next() % 40).unwrap();
            let mut end = 0;
            while end < text.len() {
                end = (end + 1 + usize::try_from(next() % 31).unwrap()).min(text.len());
                while !text.is_char_boundary(end) {
                    end += 1;
                }
                stream.text = text[..end].into();
                // Resize without changing text, then retry the same call with
                // a shorter prefix as the real stream does on a new attempt.
                if next() % 13 == 0 {
                    width = usize::try_from(next() % 40).unwrap();
                }
                live.update(&mut rows, &stream, width);
                let mut whole = vec![Line::default()];
                whole.extend(markdown(&stream.text, width, Style::new()));
                mark_reply(&mut whole);
                whole.last_mut().unwrap().spans.push(Span::new("▍", MUTED));
                assert_eq!(
                    rows, whole,
                    "seed {seed}, width {width}, at {end}: {:?}",
                    stream.text
                );
                if next() % 19 == 0 {
                    width = usize::try_from(next() % 40).unwrap();
                    live.update(&mut rows, &stream, width);
                    let mut whole = vec![Line::default()];
                    whole.extend(markdown(&stream.text, width, Style::new()));
                    mark_reply(&mut whole);
                    whole.last_mut().unwrap().spans.push(Span::new("▍", MUTED));
                    assert_eq!(rows, whole, "resize seed {seed}, at {end}");
                    stream.attempt += 1;
                    stream.text.clear();
                    live.update(&mut rows, &stream, width);
                    assert_eq!(texts(&rows), ["", "⏺ ▍"]);
                }
            }
        }
    }

    #[test]
    fn an_open_paragraph_rewraps_only_its_unstable_suffix() {
        for (seed, chunk) in [
            ("", "plain text with spaces to wrap.  "),
            ("", "abcdefghijklmnopqrstuvwxyzabcdef"),
            ("hello ", "                                "),
            ("", "                                "),
            ("", "********************************"),
            ("", "**"),
            ("hello ", "** ** "),
            (">", "                                "),
        ] {
            let mut live = Live::default();
            let mut rows = Vec::new();
            let mut stream = protocol::view::Stream {
                call: CallId::new(3).unwrap(),
                attempt: 0,
                text: String::new(),
            };
            stream.text.push_str(seed);
            live.update(&mut rows, &stream, 100);
            for _ in 0..2000 {
                stream.text.push_str(chunk);
                let before = live.rewrapped + live.parsed;
                live.update(&mut rows, &stream, 100);
                assert!(
                    live.rewrapped + live.parsed - before <= 255 + 2 * chunk.len() + 1,
                    "chunk {chunk:?}"
                );
            }
            if chunk.bytes().any(|b| b.is_ascii_alphabetic()) {
                assert!(
                    live.kept > 500,
                    "display rows must commit before the logical newline"
                );
            }
            if chunk.bytes().all(|b| b == b' ') && seed == "hello " {
                let pending = live.continuation.as_ref().unwrap();
                assert_eq!(pending.space_count, 64_001);
                assert_eq!(
                    pending
                        .spaces
                        .iter()
                        .map(|span| span.text.len())
                        .sum::<usize>(),
                    101
                );
            }
            if seed.is_empty() && chunk.bytes().all(|b| b == b' ') {
                assert_eq!(live.leading, 64_000);
                assert_eq!(live.open, "");
                stream.text.push_str("- **tail** `code` end");
            } else {
                stream.text.push_str(" **tail** `code` end");
            }
            for width in [100, 1, 20, 300] {
                live.update(&mut rows, &stream, width);
                let mut whole = vec![Line::default()];
                whole.extend(markdown(&stream.text, width, Style::new()));
                mark_reply(&mut whole);
                whole.last_mut().unwrap().spans.push(Span::new("▍", MUTED));
                assert_eq!(rows, whole, "chunk {chunk:?}, width {width}");
            }
        }
    }

    #[test]
    fn markdown_rows_are_headed_bulleted_and_fenced() {
        let rows = markdown(
            "# Title\n- one **two** `x`\n```\nfn a()\n```",
            40,
            Style::new(),
        );
        assert_eq!(
            texts(&rows),
            ["  Title", "  • one two x", "  ```", "  fn a()", "  ```"]
        );
        assert!(rows[0].spans[1].style.mods.contains(Modifier::BOLD));
        let x = rows[1].spans.iter().find(|s| s.text == "x").unwrap();
        assert_eq!(x.style, CODE);
    }

    /// The shell marks a failed command's card: no client reads its exit
    /// code out of the output.
    #[test]
    fn command_output_escapes_are_stripped_and_a_bad_mark_shows_red() {
        let done = Outcome::Done {
            output: Json::encode(&json!({"output": "\x1b[31mred\x1b[0m\n", "exit": 1}))
                .unwrap()
                .into(),
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
        assert_eq!(texts(&lines)[1..], ["⏺ $ ls  ✗ exit 1", "  ⎿  red"]);
        assert_eq!(lines[1].spans[3].style, BAD);
    }

    #[test]
    fn an_edit_card_shows_its_diff() {
        let item = Item::Tool {
            invoke: None,
            call: CallId::new(7).unwrap(),
            tool: "edit".into(),
            input: Json::encode(&json!({"path": "a.txt", "edits": []})).unwrap(),
            phase: Phase::Running,
            reason: None,
            outcome: Some(Outcome::Done {
                output: Json::encode(&json!({"path": "a.txt"})).unwrap().into(),
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
                "⏺ edit a.txt  ✓ +1 -1",
                "  ⎿  @@ -2,1 +2,1 @@",
                "     -two",
                "     +2"
            ]
        );
        let color = |row: usize| lines[row].spans[1].style;
        assert_eq!((color(3), color(4)), (BAD, OK));
    }

    #[test]
    fn a_slash_command_shows_as_a_card() {
        let mut item = Item::Tool {
            invoke: None,
            call: CallId::new(4).unwrap(),
            tool: "model".into(),
            input: Json::encode(&serde_json::json!({ "args": "" })).unwrap(),
            phase: Phase::Gating { at: 0 },
            outcome: None,
            reason: None,
            card: None,
            by: Audience::User,
        };
        assert!(!item.settled());
        assert_eq!(rows_of(&item, Look::default())[1], "⏺ /model  running…");
        if let Item::Tool { outcome, .. } = &mut item {
            *outcome = Some(Outcome::Failed {
                error: "no command /model".into(),
            });
        }
        assert_eq!(
            rows_of(&item, Look::default())[1..],
            ["⏺ /model  ✗ failed", "  ⎿  no command /model"]
        );
    }
}
