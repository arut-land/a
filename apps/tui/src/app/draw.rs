//! Frames: the transcript, what sits under it, and overlays.

use std::fmt::Write as _;
use std::path::Path;

use client::{End, Link};
use protocol::view::{Item, Usage, call_title, recap_line};

use super::super::render::{Canvas, Viewport, ellipsize, fit};
use super::super::text::{Action, Color, Line, SmolStr, Span, Style, sanitize, width_of, wrap};
use super::super::view::{self, ASK, MUTED, USER};
use super::{App, HELP, Overlay};

/// The input shows at most this many rows; it scrolls past them.
const INPUT_ROWS: usize = 8;

/// The rows under the transcript and over it, composed once per frame:
/// inline, `height` composes them to size the viewport and `draw` then
/// draws the same ones.
pub struct Composed {
    width: usize,
    bottom: Vec<Line>,
    caret: Option<(u16, u16)>,
    overlay: Option<Vec<Line>>,
}

impl App {
    /// Inline: the rows of the items that will not change any more, to
    /// print above the viewport; each item is taken once, in order.
    pub fn scrollback(&mut self, width: u16) -> Vec<Line> {
        let mut out = Vec::new();
        while self
            .client
            .view()
            .items
            .get(self.committed)
            .is_some_and(view::settled)
        {
            out.extend_from_slice(self.rows(self.committed, usize::from(width)));
            self.committed += 1;
        }
        out
    }

    /// Inline: the last `n` rows of the items not yet printed, as (item,
    /// row within it).
    fn live(&mut self, width: usize, n: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for i in (self.committed..self.client.view().items.len()).rev() {
            if out.len() == n {
                break;
            }
            let len = self.rows(i, width).len();
            let take = len.min(n - out.len());
            out.splice(0..0, (len - take..len).map(|row| (i, row)));
        }
        out
    }

    /// What a `width`-wide frame shows besides the transcript, composed
    /// now unless this frame composed it already.
    fn composed(&mut self, width: usize) -> Composed {
        match self.composed.take() {
            Some(composed) if composed.width == width => composed,
            _ => self.compose(width),
        }
    }

    fn compose(&mut self, width: usize) -> Composed {
        let (bottom, caret) = self.bottom(width);
        // An overlay is boxed on the full screen, bare inline.
        let overlay = match self.viewport {
            Viewport::Full => self.overlay_lines(width.saturating_sub(4)),
            Viewport::Inline => self.overlay_lines(width),
        };
        Composed {
            width,
            bottom,
            caret,
            overlay,
        }
    }

    /// The rows under the transcript (the recap, the oldest approval, a
    /// notice, the input, the status line) and the caret's (x, row).
    fn bottom(&mut self, width: usize) -> (Vec<Line>, Option<(u16, u16)>) {
        let summary = self.summary();
        let rule = self.rule(width);
        let mut out = Vec::new();
        if let Some(recap) = self
            .client
            .view()
            .recap
            .as_ref()
            .filter(|_| !self.recap_hidden)
        {
            let cyan = Style::new().fg(Color::Cyan);
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
                first.spans.push(Span::new(" ", Style::new()));
                first.spans.push(Span::button("×", MUTED, Action::Dismiss));
            }
            out.extend(rows);
        }
        let asked = summary.asked.map(|i| &self.client.view().items[i]);
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
            out.push(Line::from([
                Span::new(format!("? approve {}  ", ellipsize(&call_text, room)), ASK),
                Span::button(
                    "[ y  yes ]",
                    Style::new().fg(Color::Green).bold(),
                    Action::Approve(*call, true),
                ),
                Span::new("  ", Style::new()),
                Span::button(
                    "[ n  no ]",
                    Style::new().fg(Color::Red).bold(),
                    Action::Approve(*call, false),
                ),
            ]));
        }
        if let Some((text, style)) = &self.flash {
            out.push(Line::plain(ellipsize(text, width), *style));
        }
        out.push(rule);
        let (mut rows, (cx, cy)) = self.editor.lines(width, USER);
        if self.editor.is_empty() {
            // From the column after the caret, which would hide a first
            // letter under it.
            let hint = if asked.is_some() {
                " y approves · n denies"
            } else if self.client.view().session.is_none() {
                ""
            } else if self.client.turn().running {
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
        out.push(self.status(width, summary.reply));
        let last_col = u16::try_from(width.saturating_sub(1)).unwrap_or(u16::MAX);
        let caret = self.overlay.is_none().then(|| {
            (
                cx.min(last_col),
                u16::try_from(caret_row).unwrap_or(u16::MAX),
            )
        });
        (out, caret)
    }

    /// The rule over the input, kept from frame to frame at one width.
    fn rule(&mut self, width: usize) -> Line {
        if self.rule.width() != width {
            self.rule = Line::plain("─".repeat(width), MUTED);
        }
        self.rule.clone()
    }

    fn status(&self, width: usize, reply: Option<usize>) -> Line {
        let attached = self.client.view().session.is_some();
        let (state, style): (SmolStr, Style) = match self.client.link() {
            Link::Ended(End::Lost(why)) => (
                format!("✗ disconnected: {why}").into(),
                Style::new().fg(Color::Red),
            ),
            Link::Ended(End::Detached) => ("○ detached".into(), MUTED),
            Link::Wedged(_) => (
                "✗ the daemon is not responding".into(),
                Style::new().fg(Color::Red),
            ),
            Link::Reconnecting { .. } | Link::Draining => (
                "↻ daemon upgrading; reconnecting".into(),
                Style::new().fg(Color::Yellow),
            ),
            Link::Connecting => ("◌ connecting".into(), MUTED),
            Link::Replaying { .. } | Link::Live { .. } if !attached => {
                ("○ not attached".into(), MUTED)
            }
            Link::Replaying { .. } => ("↻ replaying".into(), Style::new().fg(Color::Blue)),
            Link::Live { .. } => ("● live".into(), Style::new().fg(Color::Green)),
        };
        let mut line = Line::from([Span::new(state, style)]);
        if let Some(session) = self.client.view().session {
            line.spans.push(Span::new(
                format!("  {}", session.short()),
                Style::new().bold(),
            ));
        }
        let model = reply.and_then(|i| match &self.client.view().items[i] {
            Item::Reply { model, .. } => Some(model.as_str()),
            _ => None,
        });
        let mut right = String::new();
        if let Some(model) = model {
            right.push_str(model);
            right.push_str("  ");
        }
        let Usage { input, output } = self.tokens;
        if input + output > 0 {
            let _ = write!(right, "↑{input} ↓{output}  ");
        }
        right.push_str("F1 help");
        let used = line.width();
        // The session's title, else its workspace, gets what room is left.
        let room = width.saturating_sub(used + width_of(&right) + 4);
        let view = self.client.view();
        if view.session.is_some() && room > 8 {
            let place = match &view.title {
                Some(title) => (sanitize(title).replace('\n', " "), Style::new()),
                None => (tilde(&view.workspace), MUTED),
            };
            line.spans.push(Span::new(
                format!("  {}", ellipsize(&place.0, room)),
                place.1,
            ));
        }
        let used = line.width();
        if used + width_of(&right) < width {
            line.spans.push(Span::new(
                " ".repeat(width - used - width_of(&right)),
                Style::new(),
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
                out.push(Line::plain("sessions", Style::new().bold()));
                if self.client.sessions().is_empty() {
                    out.push(Line::plain("  none yet", MUTED));
                }
                for (n, s) in self.listed().enumerate() {
                    let here = if Some(s.id) == self.client.view().session {
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
                        Style::new().reversed()
                    } else {
                        Style::new()
                    };
                    out.push(Line::from([Span::button(
                        fit(&text, width),
                        style,
                        Action::Pick(n),
                    )]));
                }
                out.push(Line::default());
                out.push(Line::from([
                    Span::new("enter opens  ", MUTED),
                    Span::button(
                        "[ n  new session ]",
                        Style::new().bold(),
                        Action::NewSession,
                    ),
                    Span::new("  ", MUTED),
                    Span::button("[ esc  close ]", MUTED, Action::Dismiss),
                ]));
            }
            Overlay::Help => {
                out.push(Line::plain("keys and commands", Style::new().bold()));
                let keys = HELP.iter().map(|&(key, what)| (key.to_owned(), what));
                for (key, what) in keys.chain(client::help(self.client.commands())) {
                    out.push(Line::from([
                        Span::new(fit(&key, 22), Style::new().fg(Color::Cyan)),
                        Span::new(what, Style::new()),
                    ]));
                }
                out.push(Line::default());
                out.push(Line::from([Span::button(
                    "[ any key  close ]",
                    MUTED,
                    Action::Dismiss,
                )]));
            }
            Overlay::Unresponsive { who, log } => {
                out.push(Line::plain(
                    format!("{who} is not responding"),
                    Style::new().fg(Color::Red).bold(),
                ));
                for line in log {
                    out.push(Line::plain(ellipsize(&format!("│ {line}"), width), MUTED));
                }
                out.push(Line::default());
                out.push(Line::from([
                    Span::button(
                        "[ r  restart the daemon ]",
                        Style::new().bold(),
                        Action::Restart,
                    ),
                    Span::new("  ", Style::new()),
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
        let composed = self.composed(width);
        let cap = usize::from(h / 2).max(3);
        let live = match &composed.overlay {
            Some(lines) => lines.len(),
            None => self.live(width, cap).len(),
        };
        let height = u16::try_from(composed.bottom.len() + live.min(cap))
            .unwrap_or(h)
            .min(h);
        self.composed = Some(composed);
        height
    }

    /// Draws a `w` × `h` frame on the app's canvas, which keeps the rows
    /// drawn as before and notes where buttons are; returns where the
    /// caret goes, if it shows.
    pub fn draw(&mut self, w: u16, h: u16) -> Option<(u16, u16)> {
        self.redraw = false;
        let mut canvas = std::mem::take(&mut self.canvas);
        canvas.begin(w, h);
        let caret = self.frame(&mut canvas);
        canvas.finish();
        self.canvas = canvas;
        caret
    }

    /// The last frame drawn.
    pub fn canvas(&self) -> &Canvas {
        &self.canvas
    }

    fn frame(&mut self, canvas: &mut Canvas) -> Option<(u16, u16)> {
        let (w, h) = canvas.size();
        let width = usize::from(w);
        let Composed {
            bottom,
            caret,
            overlay,
            ..
        } = self.composed(width);
        let body = h.saturating_sub(u16::try_from(bottom.len()).unwrap_or(u16::MAX));
        (self.width, self.body) = (width, body);
        match (self.viewport, overlay) {
            (Viewport::Inline, Some(lines)) => {
                let skip = lines.len().saturating_sub(usize::from(body));
                for (y, line) in (0..body).zip(lines.iter().skip(skip)) {
                    canvas.line(0, y, line);
                }
            }
            (viewport, overlay) => {
                // An overlay covers the transcript's buttons.
                canvas.inert = overlay.is_some();
                self.transcript(canvas, body, viewport);
                canvas.inert = false;
                if let Some(lines) = overlay {
                    self.boxed(canvas, body, &lines);
                }
            }
        }
        for (y, line) in (body..).zip(&bottom) {
            canvas.line(0, y, line);
        }
        caret.map(|(x, y)| (x, body + y))
    }

    fn transcript(&mut self, canvas: &mut Canvas, body: u16, viewport: Viewport) {
        let (w, _) = canvas.size();
        let (width, rows) = (usize::from(w), usize::from(body));
        let shown = match viewport {
            Viewport::Full => self.window(width, rows),
            Viewport::Inline => self.live(width, rows),
        };
        // A short transcript sits at the bottom, by the input.
        let top = u16::try_from(rows - shown.len()).unwrap_or(0);
        for (y, &(i, row)) in (top..).zip(&shown) {
            if let Some(line) = self.cached(i).get(row) {
                canvas.line(0, y, line);
            }
        }
        if self.anchor.is_some() && body > 0 {
            let more = Line::from([Span::button(
                " ↓ newer below · esc ",
                Style::new().fg(Color::Cyan).reversed(),
                Action::Bottom,
            )]);
            let x = w.saturating_sub(u16::try_from(more.width()).unwrap_or(0));
            canvas.line(x, body - 1, &more);
        }
    }

    /// Draws `lines` in a bordered box centered over the top `body` rows.
    fn boxed(&self, canvas: &mut Canvas, body: u16, lines: &[Line]) {
        let (width, _) = canvas.size();
        let w = width.min(84);
        let h = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX).min(body);
        if w < 8 || h < 3 {
            return;
        }
        let (x, y) = ((width - w) / 2, (body - h) / 2);
        let edge = "─".repeat(usize::from(w) - 2);
        canvas.line(x, y, &Line::plain(format!("╭{edge}╮"), MUTED));
        canvas.line(x, y + h - 1, &Line::plain(format!("╰{edge}╯"), MUTED));
        let shown = usize::from(h - 2);
        // A picker's selection stays in view.
        let skip = match self.overlay {
            Some(Overlay::Picker { selected }) => (selected + 3).saturating_sub(shown),
            _ => 0,
        }
        .min(lines.len().saturating_sub(shown));
        let side = Line::plain(format!("│{}│", " ".repeat(usize::from(w) - 2)), MUTED);
        for (row, line) in (y + 1..y + h - 1).zip(lines.iter().skip(skip)) {
            canvas.line(x, row, &side);
            canvas.line(x + 2, row, &clip(line, usize::from(w) - 4));
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
                text: ellipsize(&span.text, cols - used).into(),
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
