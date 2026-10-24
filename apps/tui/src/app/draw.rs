//! Frames: the transcript, what sits under it, and overlays.

use std::fmt::Write as _;
use std::path::Path;

use client::{End, Link};
use protocol::view::{Item, Usage, recap_line};

use super::super::render::{Canvas, Viewport, ellipsize, fit};
use super::super::text::{Action, Color, Line, SmolStr, Span, Style, sanitize, width_of, wrap};
use super::super::view::{MUTED, USER};
use super::{App, HELP, Overlay};

/// The input shows at most this many rows; it scrolls past them.
const INPUT_ROWS: usize = 8;

/// The working indicator's glyphs, out and back.
const SPINNER: [&str; 8] = ["✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳"];

/// The slash menu shows at most this many commands.
const MENU_ROWS: usize = 6;

/// The rows under the transcript and over it, composed once per frame:
/// inline, `height` composes them to size the viewport and `draw` then
/// draws the same ones.
pub struct Composed {
    width: usize,
    bottom: Vec<Line>,
    caret: Option<(u16, u16)>,
    overlay: Option<Vec<Line>>,
}

/// Scrollback progress belongs to the frame that delivered it: the frame
/// says how many of its scrollback lines reached the terminal, and the
/// rest are printed by the next redraw.
pub struct Frame<'a> {
    app: &'a mut App,
    committed: (usize, usize),
}

impl Frame<'_> {
    /// The first `lines` of this frame's scrollback were printed.
    pub fn printed(mut self, lines: usize) {
        if let Some(&at) = lines.checked_sub(1).and_then(|i| self.app.scrolled.get(i)) {
            self.committed = at;
        }
    }
}

impl std::ops::Deref for Frame<'_> {
    type Target = App;
    fn deref(&self) -> &App {
        self.app
    }
}

impl std::ops::DerefMut for Frame<'_> {
    fn deref_mut(&mut self) -> &mut App {
        self.app
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        (self.app.committed, self.app.committed_row) = self.committed;
        self.app.scrolled.clear();
    }
}

impl App {
    pub fn begin_frame(&mut self) -> Frame<'_> {
        Frame {
            committed: (self.committed, self.committed_row),
            app: self,
        }
    }

    /// Inline: the rows of the items that will not change any more, to
    /// print above the viewport; each row is taken once, in order. Until
    /// the frame says otherwise they count as printed.
    pub fn scrollback(&mut self, width: u16) -> Vec<Line> {
        let mut out = Vec::new();
        self.scrolled.clear();
        if self.verbose {
            return out;
        }
        while self
            .client
            .view()
            .items
            .get(self.committed)
            .is_some_and(Item::settled)
        {
            let (i, printed) = (self.committed, self.committed_row);
            let rows = self.rows(i, usize::from(width));
            // A row cursor kept across a resize may point past the rewrap.
            let from = printed.min(rows.len());
            out.extend_from_slice(&rows[from..]);
            let len = rows.len();
            self.scrolled
                .extend((from + 1..len).map(|row| (i, row)).chain([(i + 1, 0)]));
            if from == len {
                // A settled item without rows still moves the cursor on.
                self.scrolled.pop();
            }
            (self.committed, self.committed_row) = (i + 1, 0);
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
            let printed = if i == self.committed {
                self.committed_row.min(len)
            } else {
                0
            };
            let take = (len - printed).min(n - out.len());
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
        let working = self.working_row(width);
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
        let asked = summary.asked.is_some();
        if let Some((text, style)) = &self.flash {
            out.push(Line::plain(ellipsize(text, width), *style));
        }
        out.extend(working);
        out.extend(self.menu_rows(width));
        out.push(rule.clone());
        let (rows, (cx, cy)) = self.editor.lines(width, USER);
        // The caret's row stays in view.
        let skip = usize::from(cy).saturating_sub(INPUT_ROWS - 1);
        let caret_row = out.len() + usize::from(cy) - skip;
        out.extend(rows.into_iter().skip(skip).take(INPUT_ROWS));
        out.push(rule);
        let hint = if asked {
            "↑/↓ choose · enter confirms · 1/y yes · 2/n/esc no"
        } else if self.client.turn().running {
            "esc stops it · ? help"
        } else {
            "message the agent · / commands · ? help"
        };
        out.push(Line::plain(ellipsize(hint, width), MUTED));
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

    /// `✢ Working… (3s · ↓ 120 tokens · esc to interrupt)`, while a turn
    /// works; its glyph turns with the clock.
    fn working_row(&mut self, width: usize) -> Option<Line> {
        if !self.working() {
            return None;
        }
        let spent = self.tokens.output.saturating_sub(self.clock.before);
        let elapsed = self.clock.elapsed();
        let secs = elapsed.as_secs();
        let glyph =
            SPINNER[usize::try_from(elapsed.as_millis() / 125).unwrap_or(0) % SPINNER.len()];
        let mut note = if secs >= 60 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        };
        if spent > 0 {
            let _ = write!(note, " · ↓ {spent} tokens");
        }
        note.push_str(" · esc to interrupt");
        Some(Line::from([
            Span::new(format!("{glyph} "), Style::new().fg(Color::Yellow)),
            Span::new(
                ellipsize(&format!("Working… ({note})"), width.saturating_sub(2)),
                MUTED,
            ),
        ]))
    }

    /// The slash commands the input's first word may become, as
    /// `(name, usage, help)`; empty once the input is more than that word.
    pub(super) fn menu(&self) -> Vec<(String, String, String)> {
        let typed = self.editor.text();
        if self.menu_at.is_none() || !typed.starts_with('/') || typed.contains(char::is_whitespace)
        {
            return Vec::new();
        }
        client::help(self.client.commands())
            .filter_map(|(usage, help)| {
                let name = usage.split(' ').next()?.to_owned();
                name.starts_with(typed)
                    .then(|| (name, usage, help.to_owned()))
            })
            .collect()
    }

    /// The menu over the input, a window of `MENU_ROWS` around the
    /// selection, as Claude Code lists commands over its prompt.
    fn menu_rows(&self, width: usize) -> Vec<Line> {
        let entries = self.menu();
        let at = self
            .menu_at
            .unwrap_or(0)
            .min(entries.len().saturating_sub(1));
        let start = (at + 1).saturating_sub(MENU_ROWS);
        entries
            .iter()
            .enumerate()
            .skip(start)
            .take(MENU_ROWS)
            .map(|(n, (_, usage, help))| {
                let (mark, name) = if n == at {
                    ("❯ ", Style::new().fg(Color::Cyan).bold())
                } else {
                    ("  ", Style::new().fg(Color::Cyan))
                };
                let room = width.saturating_sub(26);
                Line::from([
                    Span::new(mark, name),
                    Span::new(fit(usage, 24), name),
                    Span::new(ellipsize(help, room), MUTED),
                ])
            })
            .collect()
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
            let place = match view.side("title") {
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
    /// is under them. Expanded transcript review uses the whole viewport.
    pub fn height(&mut self, w: u16, h: u16) -> u16 {
        if self.viewport == Viewport::Full || self.verbose || self.client.turn().asking.is_some() {
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
            Viewport::Inline
                if !self.verbose
                    && self.client.turn().asking.is_none()
                    && self.anchor.is_none() =>
            {
                self.live(width, rows)
            }
            Viewport::Full | Viewport::Inline => self.window(width, rows),
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
                " Jump to bottom (click) ↓ ",
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
