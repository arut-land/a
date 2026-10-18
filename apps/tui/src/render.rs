//! A cell buffer and the ANSI that turns the last frame into the next: only
//! changed cells are written, inside one synchronized update. The same
//! screen draws the alternate buffer (absolute moves) or an inline viewport
//! at the bottom of the normal buffer (relative moves, with finished lines
//! printed above it into the terminal's own scrollback).

use std::io::Write as _;

use super::text::{Line, char_width, width_of};

/// Theme colors only (the terminal's palette entries 1–7 and bright black),
/// never a fixed RGB, and no backgrounds but reverse video: legible on
/// light and dark themes alike.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
}

/// SGR attributes are independent flags.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub reverse: bool,
}

impl Style {
    pub const PLAIN: Self = Self {
        fg: Color::Default,
        bold: false,
        dim: false,
        italic: false,
        reverse: false,
    };

    pub const fn fg(fg: Color) -> Self {
        Self { fg, ..Self::PLAIN }
    }

    pub const fn bold(self) -> Self {
        Self { bold: true, ..self }
    }

    pub const fn dim(self) -> Self {
        Self { dim: true, ..self }
    }

    pub const fn italic(self) -> Self {
        Self {
            italic: true,
            ..self
        }
    }

    pub const fn reverse(self) -> Self {
        Self {
            reverse: true,
            ..self
        }
    }

    fn sgr(self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[0");
        for (on, code) in [
            (self.bold, &b";1"[..]),
            (self.dim, b";2"),
            (self.italic, b";3"),
            (self.reverse, b";7"),
        ] {
            if on {
                out.extend_from_slice(code);
            }
        }
        let fg: &[u8] = match self.fg {
            Color::Default => b"",
            Color::Red => b";31",
            Color::Green => b";32",
            Color::Yellow => b";33",
            Color::Blue => b";34",
            Color::Magenta => b";35",
            Color::Cyan => b";36",
            Color::Gray => b";90",
        };
        out.extend_from_slice(fg);
        out.push(b'm');
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    /// `'\0'` is the second column of a wide character.
    ch: char,
    style: Style,
}

const BLANK: Cell = Cell {
    ch: ' ',
    style: Style::PLAIN,
};

/// One frame: `w` × `h` cells and where the cursor goes.
pub struct Buf {
    pub w: u16,
    pub h: u16,
    cells: Vec<Cell>,
    /// Shown here after the frame (the input caret); hidden when `None`.
    pub cursor: Option<(u16, u16)>,
}

impl Buf {
    pub fn new(w: u16, h: u16) -> Self {
        Self {
            w,
            h,
            cells: vec![BLANK; usize::from(w) * usize::from(h)],
            cursor: None,
        }
    }

    fn clear(&mut self) {
        self.cells.fill(BLANK);
        self.cursor = None;
    }

    /// Writes `text` from column `x` of row `y`, clipped at the right
    /// edge; returns the column after it.
    pub fn put(&mut self, mut x: u16, y: u16, text: &str, style: Style) -> u16 {
        if y >= self.h {
            return x;
        }
        let row = usize::from(y) * usize::from(self.w);
        for ch in text.chars() {
            let w = char_width(ch);
            if w == 0 {
                continue;
            }
            if x + w > self.w {
                break;
            }
            let at = row + usize::from(x);
            self.cells[at] = Cell { ch, style };
            if w == 2 {
                self.cells[at + 1] = Cell { ch: '\0', style };
            }
            x += w;
        }
        x
    }

    /// The rows as text, trailing blanks trimmed: what a test asserts on.
    #[cfg(test)]
    pub fn rows(&self) -> Vec<String> {
        self.cells
            .chunks(usize::from(self.w).max(1))
            .map(|row| {
                let s: String = row.iter().map(|c| c.ch).filter(|&c| c != '\0').collect();
                s.trim_end().to_owned()
            })
            .collect()
    }

    /// The style of the cell at (x, y), for tests.
    #[cfg(test)]
    pub fn style_at(&self, x: u16, y: u16) -> Style {
        self.cells[usize::from(y) * usize::from(self.w) + usize::from(x)].style
    }
}

/// Where frames go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewport {
    /// The whole alternate screen.
    Full,
    /// The last rows of the normal buffer, under the scrollback.
    Inline,
}

pub struct Screen {
    viewport: Viewport,
    prev: Option<Buf>,
    spare: Option<Buf>,
    out: Vec<u8>,
    /// Inline: the cursor's row within the viewport, and the viewport's
    /// height on screen (rows reserved so far).
    row: u16,
    reserved: u16,
}

impl Screen {
    pub fn new(viewport: Viewport) -> Self {
        Self {
            viewport,
            prev: None,
            spare: None,
            out: Vec::with_capacity(1 << 14),
            row: 0,
            reserved: 1,
        }
    }

    /// A blank frame to draw the next one into.
    pub fn frame(&mut self, w: u16, h: u16) -> Buf {
        match self.spare.take() {
            Some(mut buf) if buf.w == w && buf.h == h => {
                buf.clear();
                buf
            }
            _ => Buf::new(w, h),
        }
    }

    /// The next frame is drawn whole (after a resize or a redraw request).
    pub fn invalidate(&mut self) {
        self.prev = None;
    }

    fn goto(&mut self, x: u16, y: u16) {
        match self.viewport {
            Viewport::Full => {
                let _ = write!(self.out, "\x1b[{};{}H", y + 1, x + 1);
            }
            Viewport::Inline => {
                if y < self.row {
                    let _ = write!(self.out, "\x1b[{}A", self.row - y);
                } else if y > self.row {
                    let _ = write!(self.out, "\x1b[{}B", y - self.row);
                }
                let _ = write!(self.out, "\x1b[{}G", x + 1);
                self.row = y;
            }
        }
    }

    /// Inline: grows the viewport to `h` rows by scrolling, so it ends on
    /// the cursor's row, and moves to its top.
    fn reserve(&mut self, h: u16) {
        if h > self.reserved {
            self.goto(0, self.reserved - 1);
            for _ in self.reserved..h {
                self.out.extend_from_slice(b"\r\n");
            }
            self.row = h - 1;
            self.reserved = h;
            self.prev = None;
        }
    }

    /// The bytes that turn the previous frame into `next`.
    pub fn render(&mut self, next: Buf) -> &[u8] {
        self.out.clear();
        self.out.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        if self.viewport == Viewport::Inline {
            self.reserve(next.h);
        }
        let prev = self.prev.take().filter(|p| p.w == next.w && p.h == next.h);
        if prev.is_none() {
            match self.viewport {
                Viewport::Full => self.out.extend_from_slice(b"\x1b[0m\x1b[2J"),
                Viewport::Inline => {
                    self.goto(0, 0);
                    self.out.extend_from_slice(b"\x1b[0m\x1b[J");
                }
            }
        }
        let mut at: Option<(u16, u16)> = None;
        let mut style: Option<Style> = None;
        let w = usize::from(next.w);
        for y in 0..next.h {
            let row = usize::from(y) * w;
            for x in 0..next.w {
                let i = row + usize::from(x);
                let cell = next.cells[i];
                if cell.ch == '\0' {
                    continue;
                }
                let unchanged = match &prev {
                    // A wide character's second half counts as part of it.
                    Some(p) => {
                        p.cells[i] == cell
                            && (char_width(cell.ch) < 2 || p.cells[i + 1] == next.cells[i + 1])
                    }
                    None => cell == BLANK,
                };
                if unchanged {
                    continue;
                }
                if at != Some((x, y)) {
                    self.goto(x, y);
                }
                if style != Some(cell.style) {
                    cell.style.sgr(&mut self.out);
                    style = Some(cell.style);
                }
                let mut utf8 = [0; 4];
                self.out
                    .extend_from_slice(cell.ch.encode_utf8(&mut utf8).as_bytes());
                at = Some((x + char_width(cell.ch), y));
            }
        }
        self.out.extend_from_slice(b"\x1b[0m");
        if let Some((x, y)) = next.cursor {
            self.goto(x, y);
            self.out.extend_from_slice(b"\x1b[?25h");
        } else if self.viewport == Viewport::Inline {
            // Relative moves need a known row; park on the last one.
            self.goto(0, next.h - 1);
        }
        self.out.extend_from_slice(b"\x1b[?2026l");
        self.spare = prev;
        self.prev = Some(next);
        &self.out
    }

    /// Inline: prints finished `lines` above the viewport, into the
    /// terminal's scrollback; the viewport is drawn whole again under them.
    pub fn scrollback(&mut self, lines: &[Line]) -> &[u8] {
        self.out.clear();
        if self.viewport != Viewport::Inline || lines.is_empty() {
            return &self.out;
        }
        self.goto(0, 0);
        self.out.extend_from_slice(b"\x1b[0m\x1b[J");
        for line in lines {
            for span in &line.spans {
                span.style.sgr(&mut self.out);
                self.out.extend_from_slice(span.text.as_bytes());
            }
            self.out.extend_from_slice(b"\x1b[0m\r\n");
        }
        self.row = 0;
        self.reserved = 1;
        self.prev = None;
        &self.out
    }

    /// Inline: clears the viewport and leaves the cursor at its top, for
    /// the shell's prompt.
    pub fn close(&mut self) -> &[u8] {
        self.out.clear();
        if self.viewport == Viewport::Inline {
            self.goto(0, 0);
            self.out.extend_from_slice(b"\x1b[0m\x1b[J\x1b[?25h");
        }
        &self.out
    }
}

/// Pads or clips `text` to exactly `cols` columns.
pub fn fit(text: &str, cols: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut w = 0;
    for ch in text.chars() {
        let cw = usize::from(char_width(ch));
        if w + cw > cols {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.extend(std::iter::repeat_n(' ', cols - w));
    out
}

/// `text` shortened to `cols` columns with an ellipsis.
pub fn ellipsize(text: &str, cols: usize) -> String {
    if width_of(text) <= cols {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in text.chars() {
        let cw = usize::from(char_width(ch));
        if w + cw + 1 > cols {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(screen: &mut Screen, text: &str) -> String {
        let mut buf = screen.frame(10, 2);
        buf.put(0, 0, text, Style::PLAIN);
        String::from_utf8(screen.render(buf).to_vec()).unwrap()
    }

    #[test]
    fn only_changed_cells_are_written() {
        let mut screen = Screen::new(Viewport::Full);
        let first = frame(&mut screen, "hello");
        assert!(first.contains("\x1b[2J") && first.contains("hello"));
        let second = frame(&mut screen, "help");
        assert!(!second.contains("\x1b[2J"));
        assert!(second.contains("\x1b[1;4H\x1b[0mp \x1b[0m"), "{second:?}");
        assert!(!second.contains("hel"));
        let third = frame(&mut screen, "help");
        assert!(!third.contains('p'), "{third:?}");
    }

    #[test]
    fn wide_characters_take_two_columns() {
        let mut buf = Buf::new(6, 1);
        assert_eq!(buf.put(0, 0, "日本語", Style::PLAIN), 6);
        assert_eq!(buf.rows(), ["日本語"]);
        let mut buf = Buf::new(5, 1);
        assert_eq!(buf.put(0, 0, "日本語", Style::PLAIN), 4);
    }

    #[test]
    fn the_inline_viewport_moves_relatively_and_prints_scrollback_above() {
        let mut screen = Screen::new(Viewport::Inline);
        let first = frame(&mut screen, "hi");
        assert!(!first.contains('H'), "absolute move in {first:?}");
        assert!(first.contains("\r\n"), "reserves its rows: {first:?}");
        let line = Line::plain("done", Style::PLAIN);
        let out = String::from_utf8(screen.scrollback(&[line]).to_vec()).unwrap();
        assert!(out.contains("done\x1b[0m\r\n"), "{out:?}");
    }

    #[test]
    fn fit_and_ellipsize_count_columns() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("日本語", 5), "日本 ");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
    }
}
