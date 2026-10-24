//! Frames on the terminal: the cells each frame changed, as ANSI, in one
//! write. The full screen is the alternate buffer, addressed absolutely.
//! Inline is a viewport at the bottom of the normal buffer, as Codex does
//! it: it grows by scrolling the screen, and finished lines are printed
//! above it into the terminal's own scrollback. Its rows are reached by
//! moves relative to the cursor, which stays where it was left, so the
//! terminal is never asked where the cursor is.

use std::io::{self, Write};
use std::ops::Range;
pub use sys::terminal::Stdout;

use super::app::App;
use super::render::{Cell, Grid, Viewport};
use super::text::{Line, Modifier, Style};

/// The pen the terminal starts from and is left at.
const PLAIN: Style = Style::new();

/// Opens and closes a synchronized update: the terminal shows the frame
/// at once, not as it arrives.
const OPEN: &[u8] = b"\x1b[?2026h";
const CLOSE: &[u8] = b"\x1b[?2026l";

pub struct Screen<W = Stdout> {
    out: W,
    /// The frame being written.
    bytes: Vec<u8>,
    viewport: Viewport,
    /// The size frames were last drawn at.
    size: (u16, u16),
    /// The cursor's row (inline: counted from the viewport's top) and
    /// column, where known: after the last column it waits to wrap.
    row: Option<u16>,
    col: Option<u16>,
    pen: Style,
    /// Colours are drawn; `NO_COLOR` keeps only the attributes.
    colour: bool,
    cursor_shown: bool,
    /// Draw the next frame whole: the first, after a resize, Ctrl-L, or
    /// (inline) when the viewport moved or changed size.
    stale: bool,
    /// Inline: scrollback lines taken from the app but not yet accepted
    /// by the terminal, printed first by the next frame at whatever width
    /// (the terminal wraps them), so a cut write neither loses nor repeats
    /// a row.
    unprinted: Vec<Line>,
    /// Where each line of `unprinted` ends in `bytes`; the next frame
    /// overwrites a partial one.
    history_ends: Vec<usize>,
    /// Inline: the viewport row the cursor is on once `bytes` up to each
    /// offset are written, so a cut-short frame resumes from where the
    /// cursor actually stopped.
    marks: Vec<(usize, Option<u16>)>,
}

impl<W: Write> Screen<W> {
    pub fn new(viewport: Viewport, out: W) -> Self {
        Self {
            out,
            bytes: Vec::with_capacity(1 << 14),
            viewport,
            size: (0, 0),
            row: Some(0),
            col: None,
            pen: PLAIN,
            colour: true,
            cursor_shown: true,
            stale: true,
            unprinted: Vec::new(),
            history_ends: Vec::new(),
            marks: Vec::new(),
        }
    }

    /// Draws bold, dim, reverse and the rest, but no colour.
    pub fn without_colour(self) -> Self {
        Self {
            colour: false,
            ..self
        }
    }

    /// The next frame is drawn whole (Ctrl-L, or a resize).
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    /// The shell or a partially written frame may have moved the cursor.
    pub fn resume(&mut self) {
        self.bytes.clear();
        self.bytes.extend_from_slice(b"\x1b[0m\x1b[?25h");
        (self.row, self.col) = (Some(0), None);
        self.pen = PLAIN;
        self.cursor_shown = true;
        self.stale = true;
    }

    /// Draws `app` on a `cols` × `rows` terminal: inline, its settled items
    /// go into the scrollback first. A frame that changed nothing (an ack,
    /// a list nothing shows) writes nothing, not even the envelope.
    pub fn draw(&mut self, app: &mut App, size: (u16, u16)) -> io::Result<()> {
        self.draw_frame(app, size)
    }

    fn draw_frame(&mut self, app: &mut App, (cols, rows): (u16, u16)) -> io::Result<()> {
        self.marks.clear();
        self.marks.push((0, self.row));
        self.bytes.extend_from_slice(OPEN);
        let (w, h) = match self.viewport {
            Viewport::Full => {
                if self.stale || self.size != (cols, rows) {
                    self.repen(PLAIN);
                    self.bytes.extend_from_slice(b"\x1b[H\x1b[2J");
                    (self.row, self.col) = (Some(0), Some(0));
                    self.stale = true;
                }
                (cols, rows)
            }
            Viewport::Inline => {
                let w = cols.max(10);
                let mut lines = std::mem::take(&mut self.unprinted);
                lines.extend(app.scrollback(w));
                if !lines.is_empty() {
                    self.history(lines);
                }
                let h = app.height(w, rows.max(4)).clamp(1, rows.max(1));
                self.place(w, h);
                (w, h)
            }
        };
        self.size = (w, h);
        let caret = app.draw(w, h);
        let canvas = app.canvas();
        let grid = canvas.grid();
        if self.stale {
            // Onto a cleared screen: blank cells need no writing.
            for y in 0..h {
                self.cells(grid, y, 0..w, true);
            }
        } else {
            for (y, cols) in canvas.changed() {
                self.cells(grid, *y, cols.clone(), false);
            }
        }
        self.stale = false;
        self.repen(PLAIN);
        // Inline, where the cursor waits is the viewport's anchor: the
        // caret, else the last row.
        let (x, y) = caret.unwrap_or((0, h.saturating_sub(1)));
        if caret.is_some() || self.viewport == Viewport::Inline {
            self.move_to(x, y);
        }
        if caret.is_some() != self.cursor_shown {
            self.cursor_shown = caret.is_some();
            let mode: &[u8] = if self.cursor_shown {
                b"\x1b[?25h"
            } else {
                b"\x1b[?25l"
            };
            self.bytes.extend_from_slice(mode);
        }
        if self.bytes.len() == OPEN.len() {
            self.bytes.clear();
            self.marks.clear();
            return Ok(());
        }
        self.bytes.extend_from_slice(CLOSE);
        self.flush()
    }

    /// Inline: clears the viewport and leaves the cursor at its top, for
    /// the shell's prompt.
    pub fn close(&mut self) -> io::Result<()> {
        if self.viewport == Viewport::Inline {
            self.home();
            self.repen(PLAIN);
            self.bytes.extend_from_slice(b"\x1b[J\x1b[?25h");
        }
        self.flush()
    }

    /// The frame's bytes, written in as many calls as the terminal takes.
    fn flush(&mut self) -> io::Result<()> {
        let mut sent = 0;
        let written = loop {
            if sent == self.bytes.len() {
                break self.out.flush();
            }
            match self.out.write(&self.bytes[sent..]) {
                Ok(0) => break Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break Err(e),
            }
        };
        let printed = self
            .history_ends
            .iter()
            .take_while(|&&end| end <= sent)
            .count();
        self.unprinted.drain(..printed);
        self.history_ends.clear();
        let reached = self.marks.iter().rev().find(|&&(at, _)| at <= sent);
        let row = reached.and_then(|&(_, row)| row);
        self.marks.clear();
        self.bytes.clear();
        if written.is_err() {
            // Planned cursor moves may never have reached the terminal. A
            // fresh anchor cannot move up into output the shell left above
            // it; inline, it is the row the delivered bytes left the cursor
            // on, so viewport rows already drawn are drawn over, not left
            // in the scrollback.
            self.resume();
            if self.viewport == Viewport::Inline && row.is_some() {
                self.row = row;
            }
        }
        written
    }

    /// Inline: prints `lines` from the viewport's top, so they scroll into
    /// the scrollback; the viewport is drawn whole again under them.
    fn history(&mut self, lines: Vec<Line>) {
        self.home();
        self.repen(PLAIN);
        self.bytes.extend_from_slice(b"\x1b[J");
        for line in &lines {
            for span in &line.spans {
                self.repen(span.style);
                self.bytes.extend_from_slice(span.text.as_bytes());
            }
            self.repen(PLAIN);
            self.bytes.extend_from_slice(b"\r\n");
            self.history_ends.push(self.bytes.len());
            // Each printed line moves the viewport's top down to the next.
            self.marks.push((self.bytes.len(), Some(0)));
        }
        // Until the write delivers them.
        self.unprinted = lines;
        // The viewport starts on the row after them.
        (self.row, self.col) = (Some(0), Some(0));
        self.stale = true;
    }

    /// Inline: makes the viewport `w` × `h` from where its top is, first
    /// scrolling the screen up for rows it would run past the bottom.
    fn place(&mut self, w: u16, h: u16) {
        if !self.stale && self.size == (w, h) {
            return;
        }
        self.home();
        self.repen(PLAIN);
        for row in 1..h {
            self.bytes.push(b'\n');
            self.marks.push((self.bytes.len(), Some(row)));
        }
        if h > 1 {
            csi(&mut self.bytes, h - 1, b'A');
            self.marks.push((self.bytes.len(), Some(0)));
        }
        self.bytes.extend_from_slice(b"\x1b[J");
        self.stale = true;
    }

    /// Inline: to column 0 of the viewport's top row.
    fn home(&mut self) {
        if let Some(row @ 1..) = self.row {
            csi(&mut self.bytes, row, b'A');
            // A cut before the column part has still changed rows.
            self.marks.push((self.bytes.len(), Some(0)));
        }
        self.bytes.push(b'\r');
        (self.row, self.col) = (Some(0), Some(0));
        self.marks.push((self.bytes.len(), self.row));
    }

    /// Writes columns `cols` of row `y` of `grid`, skipping the ones wide
    /// characters cover and, when `blank` is already on screen, blank ones.
    fn cells(&mut self, grid: &Grid, y: u16, cols: Range<u16>, blank: bool) {
        let (width, _) = grid.size();
        let row = grid.row(y);
        let mut x = cols.start;
        while x < cols.end {
            let cell = &row[usize::from(x)];
            let w = cell.width();
            if !(blank && *cell == Cell::BLANK) {
                self.move_to(x, y);
                self.repen(cell.style);
                self.bytes.extend_from_slice(cell.text.as_bytes());
                self.col = Some(x + w).filter(|&col| col < width);
            }
            x += w;
        }
    }

    fn move_to(&mut self, x: u16, y: u16) {
        if self.row == Some(y) && self.col == Some(x) {
            return;
        }
        let bytes = &mut self.bytes;
        match (self.viewport, self.row) {
            (Viewport::Full, Some(row)) if row == y => csi(bytes, x + 1, b'G'),
            (Viewport::Full, _) => {
                bytes.extend_from_slice(b"\x1b[");
                num(bytes, y + 1);
                bytes.push(b';');
                num(bytes, x + 1);
                bytes.push(b'H');
            }
            (Viewport::Inline, row) => {
                let row = row.unwrap_or(y);
                if y != row {
                    if y < row {
                        csi(bytes, row - y, b'A');
                    } else {
                        csi(bytes, y - row, b'B');
                    }
                    self.marks.push((bytes.len(), Some(y)));
                }
                if x == 0 {
                    bytes.push(b'\r');
                } else {
                    csi(bytes, x + 1, b'G');
                }
            }
        }
        (self.row, self.col) = (Some(y), Some(x));
        self.marks.push((self.bytes.len(), self.row));
    }

    /// Switches SGR state to `pen`, in one sequence.
    fn repen(&mut self, pen: Style) {
        let pen = if self.colour {
            pen
        } else {
            Style {
                fg: PLAIN.fg,
                bg: PLAIN.bg,
                ..pen
            }
        };
        if pen == self.pen {
            return;
        }
        let bytes = &mut self.bytes;
        bytes.extend_from_slice(b"\x1b[");
        let mut first = true;
        let mut param = |bytes: &mut Vec<u8>, n: u16| {
            if !std::mem::take(&mut first) {
                bytes.push(b';');
            }
            num(bytes, n);
        };
        let mut was = self.pen;
        // An attribute is only dropped by starting from plain.
        if !was.mods.minus(pen.mods).is_empty() {
            param(bytes, 0);
            was = PLAIN;
        }
        for (attr, n) in Modifier::ALL {
            if pen.mods.contains(attr) && !was.mods.contains(attr) {
                param(bytes, n);
            }
        }
        if pen.fg != was.fg {
            param(bytes, pen.fg.sgr());
        }
        if pen.bg != was.bg {
            param(bytes, pen.bg.sgr() + 10);
        }
        bytes.push(b'm');
        self.pen = pen;
    }
}

/// `ESC [ n final`.
impl Screen<Stdout> {
    pub fn close_terminal(&mut self) -> io::Result<()> {
        self.out = Stdout::restoring();
        self.close()
    }
}

fn csi(bytes: &mut Vec<u8>, n: u16, fin: u8) {
    bytes.extend_from_slice(b"\x1b[");
    num(bytes, n);
    bytes.push(fin);
}

fn num(bytes: &mut Vec<u8>, n: u16) {
    let mut digits = [0u8; 5];
    let mut at = digits.len();
    let mut n = n;
    loop {
        at -= 1;
        digits[at] = b'0' + u8::try_from(n % 10).unwrap_or(0);
        n /= 10;
        if n == 0 {
            break;
        }
    }
    bytes.extend_from_slice(&digits[at..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Color;

    fn pen(style: Style) -> Vec<u8> {
        let mut screen = Screen::new(Viewport::Full, Vec::new());
        screen.repen(style);
        screen.bytes
    }

    #[test]
    fn styles_are_one_sgr_each() {
        assert_eq!(pen(Style::new().fg(Color::Red).bold()), b"\x1b[1;31m");
        assert_eq!(pen(Style::new().fg(Color::DarkGray)), b"\x1b[90m");
        let mut screen = Screen::new(Viewport::Full, Vec::new());
        screen.repen(Style::new().fg(Color::Green).bold().dim());
        assert_eq!(screen.bytes, b"\x1b[1;2;32m");
        screen.bytes.clear();
        // Dropping an attribute starts from plain.
        screen.repen(Style::new().fg(Color::Green).dim());
        assert_eq!(screen.bytes, b"\x1b[0;2;32m");
        screen.bytes.clear();
        screen.repen(Style::new().fg(Color::Green).dim());
        assert!(screen.bytes.is_empty(), "the same pen writes nothing");
        screen.repen(PLAIN);
        assert_eq!(screen.bytes, b"\x1b[0m");
    }

    #[test]
    fn no_color_keeps_the_attributes_and_drops_the_colours() {
        let mut screen = Screen::new(Viewport::Full, Vec::new()).without_colour();
        screen.repen(Style::new().fg(Color::Red).bold());
        assert_eq!(screen.bytes, b"\x1b[1m");
        screen.bytes.clear();
        screen.repen(Style::new().fg(Color::Green).bold());
        assert!(screen.bytes.is_empty(), "a colour alone changes nothing");
    }

    #[test]
    fn a_cancelled_frame_never_moves_above_its_new_viewport_anchor() {
        struct CancelOnce(Option<Vec<u8>>);
        impl Write for CancelOnce {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if let Some(out) = &mut self.0 {
                    out.extend_from_slice(bytes);
                    Ok(bytes.len())
                } else {
                    self.0 = Some(Vec::new());
                    Err(io::ErrorKind::WouldBlock.into())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut app = App::new(Viewport::Inline, &protocol::Target::New);
        let mut screen = Screen::new(Viewport::Inline, CancelOnce(None));
        assert_eq!(
            screen.draw(&mut app, (80, 24)).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        screen.invalidate();
        screen.draw(&mut app, (80, 24)).unwrap();
        let bytes = screen.out.0.unwrap();
        let reserved = bytes.iter().position(|&byte| byte == b'\n').unwrap();
        assert!(
            !bytes[..reserved].contains(&b'A'),
            "retry moved above shell output before reserving its viewport: {bytes:?}"
        );
    }

    #[test]
    fn numbers() {
        let mut bytes = Vec::new();
        for n in [0, 7, 10, 65535] {
            num(&mut bytes, n);
            bytes.push(b' ');
        }
        assert_eq!(bytes, b"0 7 10 65535 ");
    }
}
