//! Frames on the terminal through ratatui, which diffs each against the
//! last and writes only changed cells. The full screen is ratatui's own
//! viewport. Inline is a fixed viewport this module places at the bottom of
//! the normal buffer, as Codex does: it grows by scrolling the screen,
//! and finished lines are printed above it into the terminal's own
//! scrollback. Placement is tracked, not queried, except after a resize.

use std::io::{self, BufWriter, Stdout, Write};

use crossterm::cursor::{self, MoveTo};
use crossterm::queue;
use crossterm::style::{Print, PrintStyledContent, ResetColor};
use crossterm::terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use ratatui_core::layout::Rect;
use ratatui_core::terminal::{Terminal, TerminalOptions, Viewport as Area};
use ratatui_crossterm::{CrosstermBackend, IntoCrossterm};

use super::app::App;
use super::render::Viewport;
use super::text::Line;

type Backend = CrosstermBackend<BufWriter<Stdout>>;

pub struct Screen {
    term: Terminal<Backend>,
    viewport: Viewport,
    /// Inline: where the viewport is, the row of it the cursor was left
    /// on, and whether it must be cleared and drawn whole.
    area: Rect,
    parked: u16,
    stale: bool,
}

impl Screen {
    pub fn new(viewport: Viewport) -> io::Result<Self> {
        let (term, area) = match viewport {
            Viewport::Full => (Terminal::new(backend())?, Rect::default()),
            Viewport::Inline => {
                // Asked before the input thread reads the terminal.
                let (w, h) = terminal::size()?;
                let row = super::term::cursor_row().unwrap_or(h.saturating_sub(1));
                let area = Rect::new(0, row.min(h.saturating_sub(1)), w, 1);
                (fixed(backend(), area)?, area)
            }
        };
        Ok(Self {
            term,
            viewport,
            area,
            parked: 0,
            stale: true,
        })
    }

    /// The next frame is drawn whole (Ctrl-L, or a resize). Inline, `row`
    /// is where the cursor is now, if the terminal said.
    pub fn invalidate(&mut self, row: Option<u16>) -> io::Result<()> {
        match self.viewport {
            Viewport::Full => {
                let size = self.term.size()?;
                self.term.resize(Rect::new(0, 0, size.width, size.height))
            }
            Viewport::Inline => {
                if let Some(row) = row {
                    self.area.y = row.saturating_sub(self.parked);
                }
                self.stale = true;
                Ok(())
            }
        }
    }

    /// Draws `app`: inline, its settled items go into the scrollback first.
    pub fn draw(&mut self, app: &mut App) -> io::Result<()> {
        let size = self.term.size()?;
        let (w, h) = (size.width.max(10), size.height.max(4));
        let out = self.term.backend_mut();
        queue!(out, BeginSynchronizedUpdate)?;
        if self.viewport == Viewport::Inline {
            let lines = app.scrollback(w);
            if !lines.is_empty() {
                self.history(&lines, size.height)?;
            }
            self.place(w, app.height(w, h).max(1), size.height)?;
        }
        let mut caret = None;
        self.term.draw(|frame| {
            caret = app.draw(frame.buffer_mut());
            let area = frame.area();
            if let Some((x, y)) = caret {
                frame.set_cursor_position((area.x + x, area.y + y));
            }
        })?;
        let out = self.term.backend_mut();
        if self.viewport == Viewport::Inline {
            // Where the cursor waits says where the viewport is, after a
            // resize; without a caret it waits on the last row.
            self.parked = if let Some((_, y)) = caret {
                y
            } else {
                let last = self.area.height.saturating_sub(1);
                queue!(out, MoveTo(0, self.area.y + last))?;
                last
            };
        }
        queue!(out, EndSynchronizedUpdate)?;
        out.flush()
    }

    /// Inline: prints `lines` from the viewport's top, so they scroll into
    /// the scrollback; the viewport is drawn whole again under them.
    fn history(&mut self, lines: &[Line], rows: u16) -> io::Result<()> {
        let out = self.term.backend_mut();
        queue!(
            out,
            MoveTo(0, self.area.y),
            Clear(ClearType::FromCursorDown)
        )?;
        for line in lines {
            for span in &line.spans {
                let style = span.style.into_crossterm();
                queue!(out, PrintStyledContent(style.apply(span.text.as_str())))?;
            }
            queue!(out, ResetColor, Print("\r\n"))?;
        }
        let printed = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.area.y = self
            .area
            .y
            .saturating_add(printed)
            .min(rows.saturating_sub(1));
        self.stale = true;
        Ok(())
    }

    /// Inline: makes the viewport `w` × `h`, scrolling the screen up when
    /// it would run past the bottom.
    fn place(&mut self, w: u16, h: u16, rows: u16) -> io::Result<()> {
        let h = h.min(rows);
        let mut area = Rect::new(0, self.area.y.min(rows.saturating_sub(1)), w, h);
        let out = self.term.backend_mut();
        if area.bottom() > rows {
            let up = area.bottom() - rows;
            queue!(out, MoveTo(0, rows - 1))?;
            for _ in 0..up {
                queue!(out, Print("\n"))?;
            }
            area.y = rows - h;
        }
        if area != self.area || self.stale {
            queue!(out, MoveTo(0, area.y), Clear(ClearType::FromCursorDown))?;
            self.area = area;
            self.stale = false;
            // A new terminal rather than `resize`, which clears the whole
            // screen (and moves the viewport to the top) when it narrows.
            out.flush()?;
            self.term = fixed(backend(), area)?;
        }
        Ok(())
    }

    /// Inline: clears the viewport and leaves the cursor at its top, for
    /// the shell's prompt.
    pub fn close(&mut self) -> io::Result<()> {
        let out = self.term.backend_mut();
        if self.viewport == Viewport::Inline {
            queue!(
                out,
                ResetColor,
                MoveTo(0, self.area.y),
                Clear(ClearType::FromCursorDown),
                cursor::Show
            )?;
        }
        out.flush()
    }
}

/// Stdout, unlocked between writes so the signal thread can restore the
/// terminal, and flushed once a frame.
fn backend() -> Backend {
    CrosstermBackend::new(BufWriter::with_capacity(1 << 14, io::stdout()))
}

/// A terminal that draws into `area` only, its last frame blank.
fn fixed(backend: Backend, area: Rect) -> io::Result<Terminal<Backend>> {
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Area::Fixed(area),
        },
    )
}
