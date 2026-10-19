//! The input: multi-line text with a cursor, readline keys, and history.

use ratatui_core::style::Style;

use super::keys::Key;
use super::text::{Line, Span, char_width};

#[derive(Default)]
pub struct Editor {
    text: String,
    /// A byte offset into `text`, on a char boundary.
    cursor: usize,
    history: Vec<String>,
    /// While browsing history: the entry shown, and the draft it replaced.
    browsing: Option<(usize, String)>,
}

/// What a key did to the input.
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    Changed,
    /// Enter with this text (trimmed, non-empty).
    Submit(String),
    /// The key is not an editing key.
    Ignored,
}

impl Editor {
    #[cfg(test)]
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    /// Earlier inputs, oldest first, for Up to recall.
    pub fn remember(&mut self, entry: &str) {
        let entry = entry.trim();
        if !entry.is_empty() && self.history.last().is_none_or(|last| last != entry) {
            self.history.push(entry.to_owned());
        }
    }

    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn key(&mut self, key: Key) -> Edit {
        match key {
            Key::Char(c) => {
                let mut utf8 = [0; 4];
                self.insert(c.encode_utf8(&mut utf8));
            }
            Key::Newline => self.insert("\n"),
            Key::Enter => {
                let text = self.text.trim().to_owned();
                if text.is_empty() {
                    return Edit::Ignored;
                }
                self.remember(&text);
                self.clear();
                return Edit::Submit(text);
            }
            Key::Backspace => {
                if let Some(prev) = self.prev(self.cursor) {
                    self.text.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                }
            }
            Key::Delete | Key::Ctrl('d') if self.cursor < self.text.len() => {
                let next = self.next(self.cursor);
                self.text.replace_range(self.cursor..next, "");
            }
            Key::Left | Key::Ctrl('b') => self.cursor = self.prev(self.cursor).unwrap_or(0),
            Key::Right | Key::Ctrl('f') => self.cursor = self.next(self.cursor),
            Key::WordLeft | Key::Alt('b') => self.cursor = self.word_start(),
            Key::WordRight | Key::Alt('f') => self.cursor = self.word_end(),
            Key::Home | Key::Ctrl('a') => self.cursor = self.line_start(),
            Key::End | Key::Ctrl('e') => self.cursor = self.line_end(),
            Key::Ctrl('u') => {
                let start = self.line_start();
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            Key::Ctrl('k') => {
                let end = self.line_end();
                self.text.replace_range(self.cursor..end, "");
            }
            Key::Ctrl('w') => {
                let start = self.word_start();
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            Key::Up | Key::Ctrl('p') => {
                if !self.vertical(-1) {
                    self.recall(-1);
                }
            }
            Key::Down | Key::Ctrl('n') => {
                if !self.vertical(1) {
                    self.recall(1);
                }
            }
            _ => return Edit::Ignored,
        }
        Edit::Changed
    }

    fn prev(&self, at: usize) -> Option<usize> {
        self.text[..at].char_indices().next_back().map(|(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..]
            .chars()
            .next()
            .map_or(at, |c| at + c.len_utf8())
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    fn word_start(&self) -> usize {
        let before = self.text[..self.cursor].trim_end_matches(char::is_whitespace);
        before.rfind(char::is_whitespace).map_or(0, |i| {
            i + before[i..].chars().next().map_or(1, char::len_utf8)
        })
    }

    fn word_end(&self) -> usize {
        let after = &self.text[self.cursor..];
        let skip = after.len() - after.trim_start_matches(char::is_whitespace).len();
        let rest = &after[skip..];
        self.cursor + skip + rest.find(char::is_whitespace).unwrap_or(rest.len())
    }

    /// Moves to the line above or below at the same column; false at the
    /// first or last line.
    fn vertical(&mut self, dir: i32) -> bool {
        let start = self.line_start();
        let col = self.text[start..self.cursor].chars().count();
        let target = if dir < 0 {
            if start == 0 {
                return false;
            }
            self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1)
        } else {
            let end = self.line_end();
            if end == self.text.len() {
                return false;
            }
            end + 1
        };
        let line = &self.text[target..];
        let line = &line[..line.find('\n').unwrap_or(line.len())];
        self.cursor = target + line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
        true
    }

    fn recall(&mut self, dir: i32) {
        let len = self.history.len();
        let at = match (&self.browsing, dir < 0) {
            (None, true) if len > 0 => len - 1,
            (None, _) => return,
            (Some((i, _)), true) => i.saturating_sub(1),
            (Some((i, _)), false) if i + 1 < len => i + 1,
            (Some(_), false) => {
                let (_, draft) = self.browsing.take().unwrap_or_default();
                self.text = draft;
                self.cursor = self.text.len();
                return;
            }
        };
        let draft = match self.browsing.take() {
            Some((_, draft)) => draft,
            None => std::mem::take(&mut self.text),
        };
        self.text.clone_from(&self.history[at]);
        self.cursor = self.text.len();
        self.browsing = Some((at, draft));
    }

    /// The input as rows `width` wide behind a `› ` prompt, and the
    /// cursor's (column, row) among them.
    pub fn lines(&self, width: usize, prompt: Style) -> (Vec<Line>, (u16, u16)) {
        let mut rows = Vec::new();
        let mut cursor = (2, 0);
        let mut offset = 0;
        for (n, logical) in self.text.split('\n').enumerate() {
            let first = if n == 0 {
                Span::new("› ", prompt)
            } else {
                Span::new("  ", prompt)
            };
            let wrapped = wrap_exact(logical, width.saturating_sub(2).max(1));
            let in_line = (offset..=offset + logical.len()).contains(&self.cursor);
            for (i, (start, row)) in wrapped.iter().enumerate() {
                let end = wrapped.get(i + 1).map_or(logical.len(), |(s, _)| *s);
                if in_line
                    && (offset + start..=offset + end).contains(&self.cursor)
                    && (self.cursor < offset + end || i + 1 == wrapped.len())
                {
                    let col: u16 = logical[*start..self.cursor - offset]
                        .chars()
                        .map(char_width)
                        .sum();
                    cursor = (2 + col, u16::try_from(rows.len()).unwrap_or(u16::MAX));
                }
                let pad = if i == 0 {
                    first.clone()
                } else {
                    Span::new("  ", prompt)
                };
                rows.push(Line::from(vec![pad, Span::new(row.clone(), Style::new())]));
            }
            offset += logical.len() + 1;
        }
        (rows, cursor)
    }
}

/// Splits `text` into rows of at most `width` columns, keeping every
/// character (spaces too, so the cursor maps exactly): (byte start, row).
fn wrap_exact(text: &str, width: usize) -> Vec<(usize, String)> {
    let mut rows = vec![(0, String::new())];
    let mut col = 0;
    for (i, ch) in text.char_indices() {
        let w = usize::from(char_width(ch));
        if col + w > width {
            rows.push((i, String::new()));
            col = 0;
        }
        rows.last_mut().unwrap().1.push(ch);
        col += w;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            editor.key(if c == '\n' {
                Key::Newline
            } else {
                Key::Char(c)
            });
        }
    }

    #[test]
    fn edits_and_submits() {
        let mut e = Editor::default();
        typed(&mut e, "hello wrld");
        for _ in 0..3 {
            e.key(Key::Left);
        }
        e.key(Key::Char('o'));
        assert_eq!(e.text(), "hello world");
        e.key(Key::Ctrl('w'));
        assert_eq!(e.text(), "hello rld");
        e.key(Key::End);
        assert_eq!(e.key(Key::Enter), Edit::Submit("hello rld".into()));
        assert!(e.is_empty());
        assert_eq!(e.key(Key::Enter), Edit::Ignored);
    }

    #[test]
    fn up_and_down_move_between_lines_then_through_history() {
        let mut e = Editor::default();
        e.remember("first");
        e.remember("second");
        typed(&mut e, "ab\ncd");
        e.key(Key::Up);
        e.key(Key::Char('X'));
        assert_eq!(e.text(), "abX\ncd");
        e.key(Key::Up);
        assert_eq!(e.text(), "second");
        e.key(Key::Up);
        assert_eq!(e.text(), "first");
        e.key(Key::Down);
        e.key(Key::Down);
        assert_eq!(e.text(), "abX\ncd", "the draft comes back");
    }

    #[test]
    fn lines_wrap_and_place_the_cursor() {
        let mut e = Editor::default();
        typed(&mut e, "abcdef\nx");
        let (rows, cursor) = e.lines(6, Style::new());
        let rows: Vec<String> = rows.iter().map(Line::text).collect();
        assert_eq!(rows, ["› abcd", "  ef", "  x"]);
        assert_eq!(cursor, (3, 2));
        e.key(Key::Up);
        let (_, cursor) = e.lines(6, Style::new());
        assert_eq!(cursor, (3, 0));
    }
}
