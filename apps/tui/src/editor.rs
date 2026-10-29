//! The input: multi-line text with a cursor, readline keys, and history.

use std::borrow::Cow;
use std::ops::Range;

use protocol::{Segment, Segments};

use super::keys::Key;
use super::text::{Line, Span, Style, char_width};

const LONG_PASTE: usize = 3;

#[derive(Default)]
pub struct Editor {
    text: String,
    /// A byte offset into `text`, on a char boundary.
    cursor: usize,
    /// While browsing history: the entry shown, and the draft it replaced.
    browsing: Option<(usize, String, Vec<Paste>)>,
    /// Long pastes, shown in `text` as `[Pasted text #n +m lines]` and sent
    /// as paste segments, which the model reads tagged as pasted.
    pastes: Vec<Paste>,
}

struct Paste {
    range: Range<usize>,
    text: String,
}

/// What a key did to the input.
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    Changed,
    /// Enter with this input (trimmed, non-empty).
    Submit(Segments),
    /// The key is not an editing key.
    Ignored,
}

/// History borrows its owning session until a recalled entry becomes editable.
pub enum HistoryEntry<'a> {
    Text(&'a str),
    Segments(&'a Segments),
    Command {
        tool: &'a str,
        input: &'a protocol::Json,
    },
}

impl<'a> From<HistoryEntry<'a>> for Cow<'a, str> {
    fn from(entry: HistoryEntry<'a>) -> Self {
        match entry {
            HistoryEntry::Text(text) => Self::Borrowed(text),
            HistoryEntry::Segments(segments) => Self::Owned(segments.text()),
            HistoryEntry::Command { tool, input } => {
                Self::Owned(protocol::view::command(input).map_or_else(
                    || {
                        format!("/{tool} {}", protocol::view::Item::args(input))
                            .trim_end()
                            .to_owned()
                    },
                    |command| format!("$ {command}"),
                ))
            }
        }
    }
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.pastes = Vec::new();
        self.text = String::new();
        self.cursor = 0;
        self.browsing = None;
    }

    pub fn insert(&mut self, text: &str) {
        self.replace(self.cursor..self.cursor, text);
    }

    /// Keep large pastes from pushing the conversation off screen.
    pub fn paste(&mut self, text: &str) {
        let lines = text.matches('\n').count();
        if lines < LONG_PASTE {
            return self.insert(text);
        }
        let tag = format!("[Pasted text #{} +{lines} lines]", self.pastes.len() + 1);
        self.insert(&tag);
        self.pastes.push(Paste {
            range: self.cursor - tag.len()..self.cursor,
            text: text.to_owned(),
        });
        self.pastes.sort_by_key(|p| p.range.start);
    }

    /// The input as typed text and pastes, its ends trimmed.
    fn segments(&mut self) -> Segments {
        if self.pastes.is_empty() {
            let mut text = std::mem::take(&mut self.text);
            text.truncate(text.trim_end().len());
            let start = text.len() - text.trim_start().len();
            text.drain(..start);
            return Segments(vec![Segment::Text(text)]);
        }
        let mut out = Vec::new();
        let mut at = 0;
        for paste in std::mem::take(&mut self.pastes) {
            out.push(Segment::Text(self.text[at..paste.range.start].to_owned()));
            out.push(Segment::Paste(paste.text));
            at = paste.range.end;
        }
        out.push(Segment::Text(self.text[at..].to_owned()));
        if let Some(Segment::Text(first)) = out.first_mut() {
            let start = first.len() - first.trim_start().len();
            first.drain(..start);
        }
        if let Some(Segment::Text(last)) = out.last_mut() {
            last.truncate(last.trim_end().len());
        }
        out.retain(|segment| !matches!(segment, Segment::Text(text) if text.is_empty()));
        if out.is_empty() {
            out.push(Segment::Text(String::new()));
        }
        Segments(out)
    }

    // Placeholders are atomic: editing one removes it, and literal text
    // that happens to match a label must never expand into a paste.
    fn replace(&mut self, mut range: Range<usize>, text: &str) {
        for paste in &self.pastes {
            if range.start < paste.range.end && range.end > paste.range.start
                || range.is_empty()
                    && paste.range.contains(&range.start)
                    && range.start != paste.range.start
            {
                range.start = range.start.min(paste.range.start);
                range.end = range.end.max(paste.range.end);
            }
        }
        self.pastes.retain_mut(|paste| {
            if paste.range.end <= range.start {
                true
            } else if paste.range.start >= range.end {
                let start = paste.range.start - range.len() + text.len();
                let end = paste.range.end - range.len() + text.len();
                paste.range = start..end;
                true
            } else {
                false
            }
        });
        self.text.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
    }

    pub fn recall_edge(&self, key: Key) -> bool {
        match key {
            Key::Up | Key::Ctrl('p') => self.line_start() == 0,
            Key::Down | Key::Ctrl('n') => self.line_end() == self.text.len(),
            _ => false,
        }
    }

    pub fn recalled(&mut self, text: &str) {
        self.recall(-1, &|| std::iter::once(text));
    }

    pub fn restore_draft(&mut self) {
        self.recall(1, &std::iter::empty::<&str>);
    }

    #[cfg(test)]
    pub fn key(&mut self, key: Key) -> Edit {
        self.key_with_history(key, std::iter::empty::<&str>)
    }

    pub fn key_with_history<'a, T: Into<Cow<'a, str>>, I: Iterator<Item = T>>(
        &mut self,
        key: Key,
        history: impl Fn() -> I,
    ) -> Edit {
        match key {
            Key::Char(c) => {
                let mut utf8 = [0; 4];
                self.insert(c.encode_utf8(&mut utf8));
            }
            Key::Newline => self.insert("\n"),
            Key::Enter => {
                let mut at = 0;
                let blank = self.pastes.iter().all(|paste| {
                    let blank = self.text[at..paste.range.start].trim().is_empty()
                        && paste.text.trim().is_empty();
                    at = paste.range.end;
                    blank
                }) && self.text[at..].trim().is_empty();
                if blank {
                    return Edit::Ignored;
                }
                let input = self.segments();
                self.clear();
                return Edit::Submit(input);
            }
            Key::Backspace => {
                if let Some(prev) = self.prev(self.cursor) {
                    self.replace(prev..self.cursor, "");
                }
            }
            Key::Delete | Key::Ctrl('d') if self.cursor < self.text.len() => {
                let next = self.next(self.cursor);
                self.replace(self.cursor..next, "");
            }
            Key::Left | Key::Ctrl('b') => self.cursor = self.prev(self.cursor).unwrap_or(0),
            Key::Right | Key::Ctrl('f') => self.cursor = self.next(self.cursor),
            Key::WordLeft | Key::Alt('b') => self.cursor = self.word_start(),
            Key::WordRight | Key::Alt('f') => self.cursor = self.word_end(),
            Key::Home | Key::Ctrl('a') => self.cursor = self.line_start(),
            Key::End | Key::Ctrl('e') => self.cursor = self.line_end(),
            Key::Ctrl('u') => {
                let start = self.line_start();
                self.replace(start..self.cursor, "");
            }
            Key::Ctrl('k') => {
                let end = self.line_end();
                self.replace(self.cursor..end, "");
            }
            Key::Ctrl('w') => {
                let start = self.word_start();
                self.replace(start..self.cursor, "");
            }
            Key::Up | Key::Ctrl('p') => {
                if !self.vertical(-1) {
                    self.recall(-1, &history);
                }
            }
            Key::Down | Key::Ctrl('n') => {
                if !self.vertical(1) {
                    self.recall(1, &history);
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

    fn recall<'a, T: Into<Cow<'a, str>>, I: Iterator<Item = T>>(
        &mut self,
        dir: i32,
        history: &impl Fn() -> I,
    ) {
        let entry = match (&self.browsing, dir < 0) {
            (None, true) => history().enumerate().last(),
            (None, false) => return,
            (Some((i, _, _)), true) => {
                let at = i.saturating_sub(1);
                history().nth(at).map(|text| (at, text))
            }
            (Some((i, _, _)), false) => history().nth(i + 1).map(|text| (i + 1, text)),
        };
        let Some((at, entry)) = entry else {
            if dir > 0
                && let Some((_, draft, pastes)) = self.browsing.take()
            {
                self.text = draft;
                self.pastes = pastes;
                self.cursor = self.text.len();
            }
            return;
        };
        let (draft, pastes) = match self.browsing.take() {
            Some((_, draft, pastes)) => (draft, pastes),
            None => (
                std::mem::take(&mut self.text),
                std::mem::take(&mut self.pastes),
            ),
        };
        self.pastes.clear();
        self.text = entry.into().into_owned();
        self.cursor = self.text.len();
        self.browsing = Some((at, draft, pastes));
    }

    /// The input as rows `width` wide behind a `❯ ` prompt, and the
    /// cursor's (column, row) among them.
    pub fn lines(&self, width: usize, prompt: Style) -> (Vec<Line>, (u16, u16)) {
        let mut rows = Vec::new();
        let mut cursor = (2, 0);
        let mut offset = 0;
        for (n, logical) in self.text.split('\n').enumerate() {
            let first = if n == 0 {
                Span::new("❯ ", prompt)
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
                rows.push(Line::from([pad, Span::new(row.as_str(), Style::new())]));
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

    fn sent(segments: &[Segment]) -> Edit {
        Edit::Submit(Segments(segments.to_vec()))
    }

    fn typed_text(text: &str) -> Segment {
        Segment::Text(text.into())
    }

    fn pasted(text: &str) -> Segment {
        Segment::Paste(text.into())
    }

    #[test]
    fn a_long_paste_shows_as_a_placeholder_and_is_sent_as_a_paste() {
        let mut e = Editor::default();
        e.insert("see ");
        e.paste("a\nb\nc\nd\n");
        assert_eq!(e.text(), "see [Pasted text #1 +4 lines]");
        e.paste("short\nbit");
        assert_eq!(
            e.key(Key::Enter),
            sent(&[
                typed_text("see "),
                pasted("a\nb\nc\nd\n"),
                typed_text("short\nbit")
            ])
        );
    }

    #[test]
    fn paste_labels_are_literal_unless_they_belong_to_a_paste() {
        let mut e = Editor::default();
        e.paste("a\nb\nc\nd");
        e.insert(" [Pasted text #1 +3 lines]");
        e.key(Key::Home);
        e.insert("prefix ");
        assert_eq!(
            e.key(Key::Enter),
            sent(&[
                typed_text("prefix "),
                pasted("a\nb\nc\nd"),
                typed_text(" [Pasted text #1 +3 lines]")
            ])
        );
    }

    #[test]
    fn editing_a_paste_removes_its_whole_placeholder() {
        let mut e = Editor::default();
        e.insert("before ");
        e.paste("a\nb\nc\nd");
        e.key(Key::Backspace);
        assert_eq!(e.text(), "before ");
        assert_eq!(e.key(Key::Enter), Edit::Submit("before".into()));
    }

    #[test]
    fn history_preserves_a_draft_with_pastes_and_sends_history_as_text() {
        let mut e = Editor::default();
        let history = || ["old"].into_iter();
        e.paste("a\nb\nc\nd");
        e.key_with_history(Key::Up, history);
        assert_eq!(e.text(), "old");
        e.key_with_history(Key::Down, history);
        assert_eq!(e.key(Key::Enter), sent(&[pasted("a\nb\nc\nd")]));
        e.key_with_history(Key::Up, || ["old", "a\nb\nc\nd"].into_iter());
        assert_eq!(e.text(), "a\nb\nc\nd");
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
        let history = || ["first", "second"].into_iter();
        typed(&mut e, "ab\ncd");
        e.key_with_history(Key::Up, history);
        e.key(Key::Char('X'));
        assert_eq!(e.text(), "abX\ncd");
        e.key_with_history(Key::Up, history);
        assert_eq!(e.text(), "second");
        e.key_with_history(Key::Up, history);
        assert_eq!(e.text(), "first");
        e.key_with_history(Key::Down, history);
        e.key_with_history(Key::Down, history);
        assert_eq!(e.text(), "abX\ncd", "the draft comes back");
    }

    #[test]
    fn lines_wrap_and_place_the_cursor() {
        let mut e = Editor::default();
        typed(&mut e, "abcdef\nx");
        let (rows, cursor) = e.lines(6, Style::new());
        let rows: Vec<String> = rows.iter().map(Line::text).collect();
        assert_eq!(rows, ["❯ abcd", "  ef", "  x"]);
        assert_eq!(cursor, (3, 2));
        e.key(Key::Up);
        let (_, cursor) = e.lines(6, Style::new());
        assert_eq!(cursor, (3, 0));
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn clearing_and_submitting_release_input_storage() {
        let mut editor = Editor::default();
        editor.insert(&"x".repeat(1024 * 1024));
        editor.clear();
        assert_eq!(editor.text.capacity(), 0);
        editor.insert(&"x".repeat(1024 * 1024));
        assert!(matches!(editor.key(Key::Enter), Edit::Submit(_)));
        assert_eq!(editor.text.capacity(), 0);
    }
}

#[cfg(test)]
mod growing_history_tests {
    use super::*;

    #[test]
    fn appending_history_keeps_the_browsed_entry_and_draft_stable() {
        let entries = ["first", "second", "third"];
        let len = std::cell::Cell::new(2);
        let history = || entries[..len.get()].iter().copied();
        let mut editor = Editor::default();
        editor.insert("draft");
        editor.key_with_history(Key::Up, history);
        assert_eq!(editor.text(), "second");
        len.set(3);
        editor.key_with_history(Key::Up, history);
        assert_eq!(editor.text(), "first");
        for expected in ["second", "third", "draft"] {
            editor.key_with_history(Key::Down, history);
            assert_eq!(editor.text(), expected);
        }
    }
}

#[cfg(test)]
mod blank_paste_tests {
    use super::*;

    #[test]
    fn blank_pastes_do_not_submit_their_placeholder_labels() {
        let mut editor = Editor::default();
        editor.insert("  ");
        editor.paste("\n \n \n");
        let draft = editor.text().to_owned();
        assert_eq!(editor.key(Key::Enter), Edit::Ignored);
        assert_eq!(editor.text(), draft);
        editor.insert("message");
        assert!(matches!(editor.key(Key::Enter), Edit::Submit(_)));
    }
}
