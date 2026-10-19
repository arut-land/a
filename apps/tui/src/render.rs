//! Drawing into a ratatui [`Buffer`] in coordinates relative to its area,
//! so a frame draws the same at the top of the alternate screen and in an
//! inline viewport further down; and fitting text to columns.

use ratatui_core::buffer::Buffer;
use ratatui_core::style::Style;

use super::text::{char_width, width_of};

/// Where frames go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewport {
    /// The whole alternate screen.
    Full,
    /// The last rows of the normal buffer, under the scrollback.
    Inline,
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
