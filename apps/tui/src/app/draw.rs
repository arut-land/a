//! Frames: the transcript, what sits under it, and overlays.

use std::path::Path;

use protocol::view::{Item, Link, Usage, call_title, recap_line};

use super::super::render::{Buf, Color, Style, Viewport, ellipsize, fit};
use super::super::text::{Action, Line, Span, width_of, wrap};
use super::super::view::{self, ASK, MUTED, USER};
use super::{App, Conn, HELP, Hit, Overlay};

/// The input shows at most this many rows; it scrolls past them.
const INPUT_ROWS: usize = 8;

impl App {
    /// Inline: the rows of the items that will not change any more, to
    /// print above the viewport; each item is taken once, in order.
    pub fn scrollback(&mut self, width: u16) -> Vec<Line> {
        let mut out = Vec::new();
        while self
            .view
            .items
            .get(self.committed)
            .is_some_and(view::settled)
        {
            out.extend_from_slice(self.rows(self.committed, usize::from(width)));
            self.committed += 1;
        }
        out
    }

    /// Inline: the rows of the items not yet printed.
    fn live(&mut self, width: usize) -> Vec<Line> {
        (self.committed..self.view.items.len())
            .flat_map(|i| self.rows(i, width).to_vec())
            .collect()
    }

    /// The rows under the transcript (the recap, the oldest approval, a
    /// notice, the input, the status line) and the caret's (x, row).
    fn bottom(&self, width: usize) -> (Vec<Line>, Option<(u16, u16)>) {
        let mut out = Vec::new();
        if let Some(recap) = self.view.recap.as_ref().filter(|_| !self.recap_hidden) {
            let cyan = Style::fg(Color::Cyan);
            let banner = [Span::new(recap_line(recap), cyan)];
            let first = [Span::new("↺ ", cyan)];
            let mut rows = wrap(
                &banner,
                width.saturating_sub(3),
                &first,
                &[Span::new("  ", cyan)],
            );
            rows.truncate(3);
            if let Some(first) = rows.first_mut() {
                first.spans.push(Span::new(" ", Style::PLAIN));
                first.spans.push(Span::button("×", MUTED, Action::Dismiss));
            }
            out.extend(rows);
        }
        let asked = self.view.asked().next().and_then(|call| {
            let at = self
                .view
                .items
                .binary_search_by_key(&call, Item::key)
                .ok()?;
            Some(&self.view.items[at])
        });
        if let Some(Item::Tool {
            call,
            tool,
            input,
            card,
            ..
        }) = asked
        {
            let call_text = call_title(tool, input, card.as_ref()).replace('\n', " ");
            let room = width.saturating_sub("? approve   [ y  yes ]  [ n  no ]".len());
            out.push(Line::from(vec![
                Span::new(format!("? approve {}  ", ellipsize(&call_text, room)), ASK),
                Span::button(
                    "[ y  yes ]",
                    Style::fg(Color::Green).bold(),
                    Action::Approve(*call, true),
                ),
                Span::new("  ", Style::PLAIN),
                Span::button(
                    "[ n  no ]",
                    Style::fg(Color::Red).bold(),
                    Action::Approve(*call, false),
                ),
            ]));
        }
        if let Some((text, style)) = &self.flash {
            out.push(Line::plain(ellipsize(text, width), *style));
        }
        out.push(Line::plain("─".repeat(width), MUTED));
        let (mut rows, (cx, cy)) = self.editor.lines(width, USER);
        if self.editor.is_empty() {
            // From the column after the caret, which would hide a first
            // letter under it.
            let hint = if asked.is_some() {
                " y approves · n denies"
            } else if self.view.session.is_none() {
                ""
            } else if self.running() {
                " ctrl-c stops the tool · /help"
            } else {
                " message the agent · /help"
            };
            rows[0].spans.push(Span::new(hint, MUTED));
        }
        // The caret's row stays in view.
        let skip = usize::from(cy).saturating_sub(INPUT_ROWS - 1);
        let caret_row = out.len() + usize::from(cy) - skip;
        out.extend(rows.into_iter().skip(skip).take(INPUT_ROWS));
        out.push(self.status(width));
        let last_col = u16::try_from(width.saturating_sub(1)).unwrap_or(u16::MAX);
        let caret = self.overlay.is_none().then(|| {
            (
                cx.min(last_col),
                u16::try_from(caret_row).unwrap_or(u16::MAX),
            )
        });
        (out, caret)
    }

    fn status(&self, width: usize) -> Line {
        let (state, style) = match (&self.conn, self.view.link) {
            (Conn::Lost(why), _) => (format!("✗ disconnected: {why}"), Style::fg(Color::Red)),
            (Conn::Reconnecting, _) | (_, Link::Draining) => (
                "↻ daemon upgrading; reconnecting".to_owned(),
                Style::fg(Color::Yellow),
            ),
            (Conn::Connecting, _) => ("◌ connecting".to_owned(), MUTED),
            (_, Link::Connecting) => ("○ not attached".to_owned(), MUTED),
            (_, Link::Replaying) => ("↻ replaying".to_owned(), Style::fg(Color::Blue)),
            (_, Link::Live) => ("● live".to_owned(), Style::fg(Color::Green)),
        };
        let mut left = vec![Span::new(state, style)];
        if let Some(session) = self.view.session {
            left.push(Span::new(
                format!("  {}", session.short()),
                Style::PLAIN.bold(),
            ));
        }
        let model = self.view.items.iter().rev().find_map(|item| match item {
            Item::Reply { model, .. } => Some(model.clone()),
            _ => None,
        });
        let mut right = Vec::new();
        right.extend(model);
        let Usage { input, output } = self.tokens;
        if input + output > 0 {
            right.push(format!("↑{input} ↓{output}"));
        }
        right.push("F1 help".to_owned());
        let right = right.join("  ");
        let used: usize = left.iter().map(|s| width_of(&s.text)).sum();
        // The workspace gets what room is left.
        let room = width.saturating_sub(used + width_of(&right) + 4);
        if self.view.session.is_some() && room > 8 {
            left.push(Span::new(
                format!("  {}", ellipsize(&tilde(&self.view.workspace), room)),
                MUTED,
            ));
        }
        let used: usize = left.iter().map(|s| width_of(&s.text)).sum();
        let mut line = Line::from(left);
        if used + width_of(&right) < width {
            line.spans.push(Span::new(
                " ".repeat(width - used - width_of(&right)),
                Style::PLAIN,
            ));
            line.spans.push(Span::new(right, MUTED));
        }
        line
    }

    /// The open overlay's rows.
    fn overlay_lines(&self, width: usize) -> Option<Vec<Line>> {
        let mut out = Vec::new();
        match self.overlay.as_ref()? {
            Overlay::Picker { selected } => {
                out.push(Line::plain("sessions", Style::PLAIN.bold()));
                if self.sessions.sessions.is_empty() {
                    out.push(Line::plain("  none yet", MUTED));
                }
                for (n, s) in self.listed().enumerate() {
                    let here = if Some(s.id) == self.view.session {
                        "•"
                    } else {
                        " "
                    };
                    let attached = match s.clients {
                        0 => String::new(),
                        n => format!("  · {n} attached"),
                    };
                    let text = format!(
                        "{here} {}  {:<9}  {:<10}  {:>5} entries  {}{attached}",
                        s.id.short(),
                        s.activity.name(),
                        s.residency.name(),
                        s.entries,
                        tilde(&s.workspace)
                    );
                    let style = if n == *selected {
                        Style::PLAIN.reverse()
                    } else {
                        Style::PLAIN
                    };
                    out.push(Line::from(vec![Span::button(
                        fit(&text, width),
                        style,
                        Action::Pick(n),
                    )]));
                }
                out.push(Line::default());
                out.push(Line::from(vec![
                    Span::new("enter opens  ", MUTED),
                    Span::button(
                        "[ n  new session ]",
                        Style::PLAIN.bold(),
                        Action::NewSession,
                    ),
                    Span::new("  ", MUTED),
                    Span::button("[ esc  close ]", MUTED, Action::Dismiss),
                ]));
            }
            Overlay::Help => {
                out.push(Line::plain("keys and commands", Style::PLAIN.bold()));
                for (key, what) in HELP {
                    out.push(Line::from(vec![
                        Span::new(fit(key, 22), Style::fg(Color::Cyan)),
                        Span::new(*what, Style::PLAIN),
                    ]));
                }
                out.push(Line::default());
                out.push(Line::from(vec![Span::button(
                    "[ any key  close ]",
                    MUTED,
                    Action::Dismiss,
                )]));
            }
            Overlay::Unresponsive { who, log } => {
                out.push(Line::plain(
                    format!("{who} is not responding"),
                    Style::fg(Color::Red).bold(),
                ));
                for line in log {
                    out.push(Line::plain(ellipsize(&format!("│ {line}"), width), MUTED));
                }
                out.push(Line::default());
                out.push(Line::from(vec![
                    Span::button(
                        "[ r  restart the daemon ]",
                        Style::PLAIN.bold(),
                        Action::Restart,
                    ),
                    Span::new("  ", Style::PLAIN),
                    Span::button("[ q  quit ]", MUTED, Action::Quit),
                ]));
            }
        }
        Some(out)
    }

    /// The frame height this app wants: the whole terminal on the full
    /// screen; inline, its live rows (at most half the terminal) and what
    /// is under them.
    pub fn height(&mut self, w: u16, h: u16) -> u16 {
        if self.viewport == Viewport::Full {
            return h;
        }
        let width = usize::from(w);
        let (bottom, _) = self.bottom(width);
        let live = match self.overlay_lines(width) {
            Some(lines) => lines.len(),
            None => self.live(width).len(),
        };
        let cap = usize::from(h / 2).max(3);
        u16::try_from(bottom.len() + live.min(cap))
            .unwrap_or(h)
            .min(h)
    }

    /// Draws a frame and remembers where its buttons are.
    pub fn draw(&mut self, buf: &mut Buf) {
        self.hits.clear();
        let width = usize::from(buf.w);
        let (bottom, caret) = self.bottom(width);
        let body = buf
            .h
            .saturating_sub(u16::try_from(bottom.len()).unwrap_or(u16::MAX));
        (self.width, self.body) = (width, body);
        match (self.viewport, self.overlay_lines(width.saturating_sub(4))) {
            (Viewport::Inline, Some(lines)) => {
                let skip = lines.len().saturating_sub(usize::from(body));
                for (y, line) in (0..body).zip(lines.iter().skip(skip)) {
                    self.put(buf, 0, y, line);
                }
            }
            (viewport, overlay) => {
                self.transcript(buf, body, viewport);
                if let Some(lines) = overlay {
                    // The transcript's buttons are covered.
                    self.hits.clear();
                    self.boxed(buf, body, &lines);
                }
            }
        }
        for (y, line) in (body..).zip(&bottom) {
            self.put(buf, 0, y, line);
        }
        buf.cursor = caret.map(|(x, y)| (x, body + y));
    }

    fn transcript(&mut self, buf: &mut Buf, body: u16, viewport: Viewport) {
        let (width, rows) = (usize::from(buf.w), usize::from(body));
        let lines = match viewport {
            Viewport::Full => self.window(width, rows),
            Viewport::Inline => {
                let live = self.live(width);
                live[live.len().saturating_sub(rows)..].to_vec()
            }
        };
        // A short transcript sits at the bottom, by the input.
        let top = u16::try_from(rows - lines.len()).unwrap_or(0);
        for (y, line) in (top..).zip(&lines) {
            self.put(buf, 0, y, line);
        }
        if self.anchor.is_some() && body > 0 {
            let more = Line::from(vec![Span::button(
                " ↓ newer below · esc ",
                Style::fg(Color::Cyan).reverse(),
                Action::Bottom,
            )]);
            let x = buf
                .w
                .saturating_sub(u16::try_from(more.width()).unwrap_or(0));
            self.put(buf, x, body - 1, &more);
        }
    }

    /// Draws `lines` in a bordered box centered over the top `body` rows.
    fn boxed(&mut self, buf: &mut Buf, body: u16, lines: &[Line]) {
        let w = buf.w.min(84);
        let h = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX).min(body);
        if w < 8 || h < 3 {
            return;
        }
        let (x, y) = ((buf.w - w) / 2, (body - h) / 2);
        let edge = "─".repeat(usize::from(w) - 2);
        buf.put(x, y, &format!("╭{edge}╮"), MUTED);
        buf.put(x, y + h - 1, &format!("╰{edge}╯"), MUTED);
        let shown = usize::from(h - 2);
        // A picker's selection stays in view.
        let skip = match self.overlay {
            Some(Overlay::Picker { selected }) => (selected + 3).saturating_sub(shown),
            _ => 0,
        }
        .min(lines.len().saturating_sub(shown));
        let blank = " ".repeat(usize::from(w) - 2);
        for (row, line) in (y + 1..y + h - 1).zip(lines.iter().skip(skip)) {
            buf.put(x, row, &format!("│{blank}│"), MUTED);
            self.put(buf, x + 2, row, &clip(line, usize::from(w) - 4));
        }
    }

    /// Draws a line and records its buttons.
    fn put(&mut self, buf: &mut Buf, x: u16, y: u16, line: &Line) {
        let mut at = x;
        for span in &line.spans {
            let end = buf.put(at, y, &span.text, span.style);
            if let Some(action) = span.action {
                self.hits.push(Hit {
                    y,
                    x: (at, end),
                    action,
                });
            }
            at = end;
        }
    }
}

/// `line` cut to `cols` columns.
fn clip(line: &Line, cols: usize) -> Line {
    let mut out = Line::default();
    let mut used = 0;
    for span in &line.spans {
        let w = width_of(&span.text);
        if used + w > cols {
            out.spans.push(Span {
                text: ellipsize(&span.text, cols - used),
                ..span.clone()
            });
            break;
        }
        out.spans.push(span.clone());
        used += w;
    }
    out
}

/// A path with the home directory as `~`.
fn tilde(path: &Path) -> String {
    let rest =
        std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_owned));
    match rest {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}
