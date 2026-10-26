//! The cell [`Grid`] a frame is drawn into, the [`Canvas`] that draws rows
//! onto it and redraws only the rows that changed, and fitting text to
//! columns. Frames are drawn from row 0, so one draws the same on the
//! alternate screen and in an inline viewport further down.

use std::ops::{Index, Range};

use super::text::{Action, Line, SmolStr, Style, char_width, width_of};

pub use crate::Viewport;

/// One cell: the character drawn there with the zero-width marks that
/// follow it (an accent, a variation selector), and its pen. A wide
/// character's second column is a blank cell the screen skips.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text: SmolStr,
    pub style: Style,
}

impl Cell {
    pub const BLANK: Self = Self {
        text: SmolStr::new_inline(" "),
        style: Style::new(),
    };

    /// The columns it takes: its character's, at least one.
    pub fn width(&self) -> u16 {
        if self.text.len() == 1 {
            return 1;
        }
        self.text
            .chars()
            .next()
            .map_or(1, |ch| char_width(ch).max(1))
    }
}

/// A `w` × `h` frame of cells, row-major.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grid {
    w: u16,
    h: u16,
    cells: Vec<Cell>,
}

impl Grid {
    pub fn new(w: u16, h: u16) -> Self {
        Self {
            w,
            h,
            cells: vec![Cell::BLANK; usize::from(w) * usize::from(h)],
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.w, self.h)
    }

    pub fn row(&self, y: u16) -> &[Cell] {
        let start = usize::from(y) * usize::from(self.w);
        &self.cells[start..start + usize::from(self.w)]
    }

    fn row_mut(&mut self, y: u16) -> &mut [Cell] {
        let start = usize::from(y) * usize::from(self.w);
        &mut self.cells[start..start + usize::from(self.w)]
    }

    /// Writes `text` from column `x` of row `y`, a cell a character,
    /// clipped at the right edge; returns the column after it. A
    /// zero-width character joins the cell before it (a combining mark, a
    /// variation selector), except joiners and format characters, which
    /// would make the terminal's cursor disagree with the grid's columns.
    pub fn put(&mut self, x: u16, y: u16, text: &str, style: Style) -> u16 {
        if y >= self.h || x >= self.w {
            return x;
        }
        let cells = self.row_mut(y);
        let mut at = usize::from(x);
        let mut last = None;
        for ch in text.chars() {
            let w = usize::from(char_width(ch));
            if w == 0 {
                if let Some(last) = last
                    && !ch.is_control()
                    && !matches!(ch, '\u{200b}'..='\u{200f}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
                {
                    let cell: &mut Cell = &mut cells[last];
                    cell.text = cell.text.chars().chain([ch]).collect();
                }
                continue;
            }
            if at + w > cells.len() {
                break;
            }
            let mut utf8 = [0; 4];
            cells[at] = Cell {
                text: SmolStr::new_inline(ch.encode_utf8(&mut utf8)),
                style,
            };
            // The column a wide character covers.
            for cell in &mut cells[at + 1..at + w] {
                *cell = Cell::BLANK;
            }
            last = Some(at);
            at += w;
        }
        u16::try_from(at).unwrap_or(self.w)
    }

    /// The rows as text, trailing blanks trimmed.
    pub fn rows(&self) -> Vec<String> {
        (0..self.h)
            .map(|y| {
                let mut s = String::new();
                let mut hidden = 0;
                for cell in self.row(y) {
                    // The cells a wide character covers.
                    if hidden > 0 {
                        hidden -= 1;
                        continue;
                    }
                    s.push_str(&cell.text);
                    hidden = cell.width() - 1;
                }
                s.trim_end().to_owned()
            })
            .collect()
    }
}

impl Index<(u16, u16)> for Grid {
    type Output = Cell;

    fn index(&self, (x, y): (u16, u16)) -> &Cell {
        &self.row(y)[usize::from(x)]
    }
}

/// A frame's cells, drawn by the line and redrawn by the row: a row drawn
/// with the same lines as the frame before keeps its cells, so a streamed
/// token costs the rows it changed rather than the screen, and only those
/// rows are compared to find what the terminal is sent.
#[derive(Default)]
pub struct Canvas {
    grid: Grid,
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
        if self.grid.size() != (w, h) {
            self.grid = Grid::new(w, h);
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

    /// The frame's columns and rows.
    pub fn size(&self) -> (u16, u16) {
        self.grid.size()
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
        for (y, row) in (0..).zip(&mut self.rows) {
            if row.kept < row.ops.len() {
                row.ops.truncate(row.kept);
                row.dirty = true;
            }
            if !row.dirty {
                continue;
            }
            let cells = self.grid.row_mut(y);
            self.before.clear();
            self.before.extend_from_slice(cells);
            cells.fill(Cell::BLANK);
            for op in &mut row.ops {
                op.hits.clear();
                let mut at = op.x;
                for span in &op.line.spans {
                    let end = self.grid.put(at, y, &span.text, span.style);
                    if let Some(action) = span.action {
                        op.hits.push((at..end, action));
                    }
                    at = end;
                }
            }
            if let Some(cols) = changed(&self.before, self.grid.row(y)) {
                self.changed.push((y, cols));
            }
        }
    }

    /// The frame's cells.
    pub fn grid(&self) -> &Grid {
        &self.grid
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

/// The columns from the first to the last cell that differ, starting at a
/// wide character rather than at the column it covers.
fn changed(before: &[Cell], after: &[Cell]) -> Option<Range<u16>> {
    let differs = |(a, b): (&Cell, &Cell)| a != b;
    let mut first = before.iter().zip(after).position(differs)?;
    let last = before.iter().zip(after).rposition(differs)?;
    if first > 0 && after[first - 1].width() > 1 {
        first -= 1;
    }
    Some(u16::try_from(first).ok()?..u16::try_from(last + 1).ok()?)
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
    use crate::text::Color;

    #[test]
    fn wide_characters_take_two_columns() {
        let mut grid = Grid::new(6, 1);
        assert_eq!(grid.put(0, 0, "日本語", Style::new()), 6);
        assert_eq!(grid.rows(), ["日本語"]);
        let mut grid = Grid::new(5, 1);
        assert_eq!(grid.put(0, 0, "日本語", Style::new()), 4);
        assert_eq!(grid.rows(), ["日本"]);
        // Drawn over a wide character, a narrow one blanks its other column.
        assert_eq!(grid.put(0, 0, "a", Style::new()), 1);
        assert_eq!(grid.rows(), ["a 本"]);
    }

    /// Text takes the columns `wrap` counted for it: a cell a character,
    /// marks riding on the cell before, joiners dropped.
    #[test]
    fn wide_and_combining_text_takes_its_columns() {
        let red = Style::new().fg(Color::Red);
        let mut grid = Grid::new(12, 2);
        assert_eq!(grid.put(0, 1, "e\u{301}e", red), 2);
        assert_eq!(grid[(0, 1)].text, "e\u{301}");
        assert_eq!(grid[(0, 1)].style, red);
        assert_eq!(grid[(1, 1)].text, "e");
        assert_eq!(grid.put(2, 1, "👨\u{200d}👩", red), 6);
        assert_eq!(grid[(2, 1)].text, "👨");
        assert_eq!(grid[(4, 1)].text, "👩");
        assert_eq!(grid.put(6, 1, "\u{301}x\x07y", red), 8);
        assert_eq!(grid[(6, 1)].text, "x", "a lone mark is dropped");
        assert_eq!(grid[(7, 1)].text, "y", "a control is dropped");
        assert_eq!(grid.rows()[0], "");
        for cell in grid.row(1) {
            assert!(cell.width() <= 2);
        }
    }

    #[test]
    fn clipped_at_the_right_edge() {
        let mut grid = Grid::new(4, 2);
        assert_eq!(grid.put(1, 1, "abcdef", Style::new()), 4);
        assert_eq!(grid.rows(), ["", " abc"]);
        assert_eq!(grid.put(4, 1, "z", Style::new()), 4);
        assert_eq!(grid.put(0, 2, "z", Style::new()), 0);
    }

    /// A redrawn row reports the columns that differ, from a wide
    /// character's first column.
    #[test]
    fn a_frame_changes_only_the_cells_it_redrew() {
        let mut canvas = Canvas::default();
        let line = |t: &str| Line::plain(t, Style::new());
        canvas.begin(8, 2);
        canvas.line(0, 0, &line("ab日x"));
        canvas.line(0, 1, &line("same"));
        canvas.finish();
        assert_eq!(canvas.changed(), [(0, 0..5), (1, 0..4)]);
        canvas.begin(8, 2);
        canvas.line(0, 0, &line("ab日y"));
        canvas.line(0, 1, &line("same"));
        canvas.finish();
        assert_eq!(canvas.changed(), [(0, 4..5)]);
        canvas.begin(8, 2);
        canvas.line(0, 0, &line("ab本y"));
        canvas.line(0, 1, &line("same"));
        canvas.finish();
        assert_eq!(canvas.changed(), [(0, 2..3)]);
        canvas.begin(8, 2);
        canvas.line(0, 0, &line("ab本y"));
        canvas.line(0, 1, &line("same"));
        canvas.finish();
        assert_eq!(canvas.changed(), []);
    }

    #[test]
    fn fit_and_ellipsize_count_columns() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("日本語", 5), "日本 ");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
    }
}
