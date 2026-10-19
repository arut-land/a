//! Frames on the terminal: the cells each frame changed, as ANSI, in one
//! write. The full screen is the alternate buffer, addressed absolutely.
//! Inline is a viewport at the bottom of the normal buffer, as Codex does
//! it: it grows by scrolling the screen, and finished lines are printed
//! above it into the terminal's own scrollback. Its rows are reached by
//! moves relative to the cursor, which stays where it was left, so the
//! terminal is never asked where the cursor is.

use std::io::{self, Write};
use std::ops::Range;
use std::os::fd::BorrowedFd;

use ratatui_core::buffer::{Buffer, Cell};
use ratatui_core::style::{Color, Modifier, Style};

use super::app::App;
use super::render::{Viewport, cell_width};
use super::text::Line;

/// SGR state: foreground, background, attributes.
type Pen = (Color, Color, Modifier);

const PLAIN: Pen = (Color::Reset, Color::Reset, Modifier::empty());

/// Attributes and the SGR parameter that sets each.
const ATTRS: [(Modifier, u16); 9] = [
    (Modifier::BOLD, 1),
    (Modifier::DIM, 2),
    (Modifier::ITALIC, 3),
    (Modifier::UNDERLINED, 4),
    (Modifier::SLOW_BLINK, 5),
    (Modifier::RAPID_BLINK, 6),
    (Modifier::REVERSED, 7),
    (Modifier::HIDDEN, 8),
    (Modifier::CROSSED_OUT, 9),
];

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
    pen: Pen,
    cursor_shown: bool,
    /// Draw the next frame whole: the first, after a resize, Ctrl-L, or
    /// (inline) when the viewport moved or changed size.
    stale: bool,
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
            cursor_shown: true,
            stale: true,
        }
    }

    /// The next frame is drawn whole (Ctrl-L, or a resize).
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    /// Draws `app` on a `cols` × `rows` terminal: inline, its settled items
    /// go into the scrollback first.
    pub fn draw(&mut self, app: &mut App, (cols, rows): (u16, u16)) -> io::Result<()> {
        self.bytes.extend_from_slice(b"\x1b[?2026h");
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
                let lines = app.scrollback(w);
                if !lines.is_empty() {
                    self.history(&lines);
                }
                let h = app.height(w, rows.max(4)).clamp(1, rows.max(1));
                self.place(w, h);
                (w, h)
            }
        };
        self.size = (w, h);
        let caret = app.draw(w, h);
        let canvas = app.canvas();
        let buf = canvas.buffer();
        if self.stale {
            // Onto a cleared screen: blank cells need no writing.
            for y in 0..h {
                self.cells(buf, y, 0..w, true);
            }
        } else {
            for (y, cols) in canvas.changed() {
                self.cells(buf, *y, cols.clone(), false);
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
        self.bytes.extend_from_slice(b"\x1b[?2026l");
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

    /// The frame's bytes, in one write.
    fn flush(&mut self) -> io::Result<()> {
        let written = self
            .out
            .write_all(&self.bytes)
            .and_then(|()| self.out.flush());
        self.bytes.clear();
        written
    }

    /// Inline: prints `lines` from the viewport's top, so they scroll into
    /// the scrollback; the viewport is drawn whole again under them.
    fn history(&mut self, lines: &[Line]) {
        self.home();
        self.repen(PLAIN);
        self.bytes.extend_from_slice(b"\x1b[J");
        for line in lines {
            for span in &line.spans {
                self.repen(pen_of(span.style));
                self.bytes.extend_from_slice(span.text.as_bytes());
            }
            self.repen(PLAIN);
            self.bytes.extend_from_slice(b"\r\n");
        }
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
        for _ in 1..h {
            self.bytes.push(b'\n');
        }
        if h > 1 {
            csi(&mut self.bytes, h - 1, b'A');
        }
        self.bytes.extend_from_slice(b"\x1b[J");
        self.stale = true;
    }

    /// Inline: to column 0 of the viewport's top row.
    fn home(&mut self) {
        if let Some(row @ 1..) = self.row {
            csi(&mut self.bytes, row, b'A');
        }
        self.bytes.push(b'\r');
        (self.row, self.col) = (Some(0), Some(0));
    }

    /// Writes columns `cols` of row `y` of `buf`, skipping the ones wide
    /// characters cover and, when `blank` is already on screen, blank ones.
    fn cells(&mut self, buf: &Buffer, y: u16, cols: Range<u16>, blank: bool) {
        let width = buf.area.width;
        let start = usize::from(y) * usize::from(width);
        let row = &buf.content[start..start + usize::from(width)];
        let mut x = cols.start;
        while x < cols.end {
            let cell = &row[usize::from(x)];
            let w = cell_width(cell);
            if !(blank && *cell == Cell::EMPTY) {
                self.move_to(x, y);
                self.repen((cell.fg, cell.bg, cell.modifier));
                self.bytes.extend_from_slice(cell.symbol().as_bytes());
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
                if y < row {
                    csi(bytes, row - y, b'A');
                } else if y > row {
                    csi(bytes, y - row, b'B');
                }
                if x == 0 {
                    bytes.push(b'\r');
                } else {
                    csi(bytes, x + 1, b'G');
                }
            }
        }
        (self.row, self.col) = (Some(y), Some(x));
    }

    /// Switches SGR state to `pen`, in one sequence.
    fn repen(&mut self, pen: Pen) {
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
        if !(was.2 - pen.2).is_empty() {
            param(bytes, 0);
            was = PLAIN;
        }
        for (attr, n) in ATTRS {
            if pen.2.contains(attr) && !was.2.contains(attr) {
                param(bytes, n);
            }
        }
        for (color, was, base) in [(pen.0, was.0, 30), (pen.1, was.1, 40)] {
            if color == was {
                continue;
            }
            match color {
                Color::Reset => param(bytes, base + 9),
                Color::Rgb(r, g, b) => {
                    for n in [base + 8, 2, r.into(), g.into(), b.into()] {
                        param(bytes, n);
                    }
                }
                Color::Indexed(i) => {
                    for n in [base + 8, 5, i.into()] {
                        param(bytes, n);
                    }
                }
                named => param(bytes, base + named_offset(named)),
            }
        }
        bytes.push(b'm');
        self.pen = pen;
    }
}

/// A named color's SGR parameter, from 30 (40 for a background).
fn named_offset(color: Color) -> u16 {
    match color {
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 60,
        Color::LightRed => 61,
        Color::LightGreen => 62,
        Color::LightYellow => 63,
        Color::LightBlue => 64,
        Color::LightMagenta => 65,
        Color::LightCyan => 66,
        Color::White => 67,
        _ => 0,
    }
}

/// The pen `style` draws with over a blank cell.
fn pen_of(style: Style) -> Pen {
    let mut cell = Cell::EMPTY;
    cell.set_style(style);
    (cell.fg, cell.bg, cell.modifier)
}

/// `ESC [ n final`.
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

/// The terminal, written to directly: each frame is one `write`, where
/// `io::Stdout`'s line buffer would split one that holds a newline.
pub struct Stdout(BorrowedFd<'static>);

impl Default for Stdout {
    fn default() -> Self {
        Self(rustix::stdio::stdout())
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(rustix::io::write(self.0, buf)?)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pen(fg: Color, bg: Color, m: Modifier) -> Vec<u8> {
        let mut screen = Screen::new(Viewport::Full, Vec::new());
        screen.repen((fg, bg, m));
        screen.bytes
    }

    #[test]
    fn styles_are_one_sgr_each() {
        assert_eq!(pen(Color::Red, Color::Reset, Modifier::BOLD), b"\x1b[1;31m");
        assert_eq!(
            pen(Color::DarkGray, Color::Reset, Modifier::empty()),
            b"\x1b[90m"
        );
        assert_eq!(
            pen(Color::Reset, Color::Blue, Modifier::REVERSED),
            b"\x1b[7;44m"
        );
        assert_eq!(
            pen(Color::Indexed(208), Color::Rgb(1, 2, 3), Modifier::empty()),
            b"\x1b[38;5;208;48;2;1;2;3m"
        );
        let mut screen = Screen::new(Viewport::Full, Vec::new());
        screen.repen((Color::Green, Color::Reset, Modifier::BOLD | Modifier::DIM));
        screen.bytes.clear();
        // Dropping an attribute starts from plain.
        screen.repen((Color::Green, Color::Reset, Modifier::DIM));
        assert_eq!(screen.bytes, b"\x1b[0;2;32m");
        screen.bytes.clear();
        screen.repen(PLAIN);
        assert_eq!(screen.bytes, b"\x1b[0m");
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
