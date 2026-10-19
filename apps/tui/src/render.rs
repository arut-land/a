//! The [`Canvas`] a frame is drawn on, which redraws only the rows that
//! changed; drawing text into a ratatui [`Buffer`]; and fitting text to
//! columns. Frames are drawn from row 0, so one draws the same on the
//! alternate screen and in an inline viewport further down.

use std::ops::Range;

use ratatui_core::buffer::{Buffer, Cell};
use ratatui_core::layout::Rect;
use ratatui_core::style::Style;
use unicode_width::UnicodeWidthStr;

use super::text::{Action, Line, char_width, width_of};

pub use crate::Viewport;

/// A frame's cells, drawn by the line and redrawn by the row: a row drawn
/// with the same lines as the frame before keeps its cells, so a streamed
/// token costs the rows it changed rather than the screen, and only those
/// rows are compared to find what the terminal is sent.
#[derive(Default)]
pub struct Canvas {
    buf: Buffer,
    rows: Vec<Row>,
    /// Lines drawn now are under an overlay: they show, but a click on
    /// them does nothing.
    pub inert: bool,
    /// What the last frame changed: rows and their columns, top down.
    changed: Vec<(u16, Range<u16>)>,
    /// A row's cells before it is redrawn.
    before: Vec<Cell>,
}

/// One row's lines in the order drawn, and how many of them this frame has
/// drawn again so far.
#[derive(Default)]
struct Row {
    ops: Vec<Op>,
    kept: usize,
    dirty: bool,
}

struct Op {
    x: u16,
    line: Line,
    live: bool,
    /// Where its clickable spans landed.
    hits: Vec<(Range<u16>, Action)>,
}

impl Canvas {
    /// Starts a `w` × `h` frame; a new size starts from blank.
    pub fn begin(&mut self, w: u16, h: u16) {
        let area = Rect::new(0, 0, w, h);
        if self.buf.area != area {
            self.buf = Buffer::empty(area);
            self.rows.clear();
            self.rows.resize_with(usize::from(h), Row::default);
        }
        for row in &mut self.rows {
            row.kept = 0;
            row.dirty = false;
        }
        self.inert = false;
        self.changed.clear();
    }

    pub fn area(&self) -> Rect {
        self.buf.area
    }

    /// Draws `line` from column `x` of row `y`, clipped at the right edge;
    /// later lines draw over earlier ones.
    pub fn line(&mut self, x: u16, y: u16, line: &Line) {
        let clickable = !self.inert;
        let Some(row) = self.rows.get_mut(usize::from(y)) else {
            return;
        };
        let same = |op: &Op| op.x == x && op.live == clickable && op.line == *line;
        if !row.dirty && row.ops.get(row.kept).is_some_and(same) {
            row.kept += 1;
            return;
        }
        row.dirty = true;
        row.ops.truncate(row.kept);
        row.ops.push(Op {
            x,
            line: line.clone(),
            live: clickable,
            hits: Vec::new(),
        });
        row.kept += 1;
    }

    /// Ends the frame: redraws the rows whose lines changed, and notes
    /// which of their cells did.
    pub fn finish(&mut self) {
        let width = usize::from(self.buf.area.width);
        for (y, row) in (0..).zip(&mut self.rows) {
            if row.kept < row.ops.len() {
                row.ops.truncate(row.kept);
                row.dirty = true;
            }
            if !row.dirty {
                continue;
            }
            let start = usize::from(y) * width;
            let cells = &mut self.buf.content[start..start + width];
            self.before.clear();
            self.before.extend_from_slice(cells);
            cells.iter_mut().for_each(Cell::reset);
            for op in &mut row.ops {
                op.hits = draw(&mut self.buf, op.x, y, &op.line);
            }
            if let Some(cols) = changed(&self.before, &self.buf.content[start..start + width]) {
                self.changed.push((y, cols));
            }
        }
    }

    /// The frame's cells.
    pub fn buffer(&self) -> &Buffer {
        &self.buf
    }

    /// The cells the last frame changed, by row.
    pub fn changed(&self) -> &[(u16, Range<u16>)] {
        &self.changed
    }

    /// What a click at (`x`, `y`) on the last frame does: the topmost
    /// clickable span there, if any.
    pub fn hit(&self, x: u16, y: u16) -> Option<Action> {
        let row = self.rows.get(usize::from(y))?;
        row.ops
            .iter()
            .rev()
            .filter(|op| op.live)
            .flat_map(|op| &op.hits)
            .find(|(cols, _)| cols.contains(&x))
            .map(|&(_, action)| action)
    }
}

/// Draws `line`'s spans from column `x` of row `y`; returns where its
/// clickable ones landed.
fn draw(buf: &mut Buffer, x: u16, y: u16, line: &Line) -> Vec<(Range<u16>, Action)> {
    let mut hits = Vec::new();
    let mut at = x;
    for span in &line.spans {
        let end = put(buf, at, y, &span.text, span.style);
        if let Some(action) = span.action {
            hits.push((at..end, action));
        }
        at = end;
    }
    hits
}

/// The columns from the first to the last cell that differ, starting at a
/// wide character rather than at the column it covers.
fn changed(before: &[Cell], after: &[Cell]) -> Option<Range<u16>> {
    let differs = |(a, b): (&Cell, &Cell)| a != b;
    let mut first = before.iter().zip(after).position(differs)?;
    let last = before.iter().zip(after).rposition(differs)?;
    if first > 0 && cell_width(&after[first - 1]) > 1 {
        first -= 1;
    }
    Some(u16::try_from(first).ok()?..u16::try_from(last + 1).ok()?)
}

/// The columns a cell's symbol takes, as ratatui counts them when placing
/// it (the next ones it covers are blank).
pub fn cell_width(cell: &Cell) -> u16 {
    let symbol = cell.symbol();
    if symbol.len() == 1 {
        return 1;
    }
    u16::try_from(symbol.width()).map_or(1, |w| w.max(1))
}

/// Writes `text` from column `x` of row `y` of `buf`'s area, clipped at its
/// right edge; returns the column after it.
pub fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style) -> u16 {
    let area = buf.area;
    if y >= area.height || x >= area.width {
        return x;
    }
    if !text.chars().all(alone) {
        let (end, _) = buf.set_stringn(area.x + x, area.y + y, text, usize::MAX, style);
        return end - area.x;
    }
    // A cell a character, as `set_stringn` would place them, without its
    // grapheme segmentation.
    let row = buf.index_of(area.x, area.y + y);
    let cells = &mut buf.content[row..row + usize::from(area.width)];
    let mut at = usize::from(x);
    for ch in text.chars() {
        let w = usize::from(char_width(ch));
        if at + w > cells.len() {
            break;
        }
        cells[at].set_char(ch).set_style(style);
        // The column a wide character covers.
        for cell in &mut cells[at + 1..at + w] {
            cell.reset();
        }
        at += w;
    }
    u16::try_from(at).unwrap_or(area.width)
}

/// Whether `ch` is a grapheme of its own whatever is around it (no mark,
/// joiner, jamo, regional indicator or control): what most text is made of.
fn alone(ch: char) -> bool {
    matches!(ch, ' '..='~')
        || matches!(
            ch,
            '\u{a0}'..='\u{5ff}'
                | '\u{2010}'..='\u{2027}'
                | '\u{2030}'..='\u{205f}'
                | '\u{2070}'..='\u{2bff}'
                | '\u{3000}'..='\u{9fff}'
                | '\u{ac00}'..='\u{d7a3}'
                | '\u{ff01}'..='\u{ff9d}'
        ) && char_width(ch) > 0
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

/// The rows of `buf` as text, trailing blanks trimmed: what a test asserts
/// on.
#[cfg(test)]
pub fn rows(buf: &Buffer) -> Vec<String> {
    let w = usize::from(buf.area.width).max(1);
    buf.content()
        .chunks(w)
        .map(|row| {
            let mut s = String::new();
            let mut hidden = 0;
            for cell in row {
                // The cells a wide character covers.
                if hidden > 0 {
                    hidden -= 1;
                    continue;
                }
                s.push_str(cell.symbol());
                hidden = width_of(cell.symbol()).saturating_sub(1);
            }
            s.trim_end().to_owned()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ratatui_core::layout::Rect;

    use super::*;

    #[test]
    fn wide_characters_take_two_columns() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
        assert_eq!(put(&mut buf, 0, 0, "日本語", Style::new()), 6);
        assert_eq!(rows(&buf), ["日本語"]);
        let mut buf = Buffer::empty(Rect::new(0, 0, 5, 1));
        assert_eq!(put(&mut buf, 0, 0, "日本語", Style::new()), 4);
    }

    #[test]
    fn cells_are_what_ratatui_would_write() {
        let red = Style::new().fg(ratatui_core::style::Color::Red);
        for text in [
            "plain ascii",
            "日本語 and ─│•▍…",
            "e\u{301}e",
            "👨‍👩‍👧",
            "🇯🇵",
            "ab日",
            "ｶﾞ",
        ] {
            for x in [0, 3, 6] {
                let area = Rect::new(2, 1, 7, 2);
                let (mut ours, mut theirs) = (Buffer::empty(area), Buffer::empty(area));
                let end = put(&mut ours, x, 1, text, red);
                let (to, _) = theirs.set_stringn(2 + x, 2, text, usize::MAX, red);
                assert_eq!((end + 2, &ours), (to, &theirs), "{text:?} at {x}");
            }
        }
    }

    #[test]
    fn coordinates_are_relative_to_the_area() {
        let mut buf = Buffer::empty(Rect::new(0, 5, 4, 2));
        assert_eq!(put(&mut buf, 1, 1, "abcdef", Style::new()), 4);
        assert_eq!(rows(&buf), ["", " abc"]);
    }

    #[test]
    fn fit_and_ellipsize_count_columns() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("日本語", 5), "日本 ");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
    }
}
