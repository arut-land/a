//! Styled text: spans, and word wrapping by display width.

use unicode_width::UnicodeWidthChar;

use ratatui_core::style::Style;

/// What a click on a span does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Collapses or expands tool call `call`'s card.
    Toggle(protocol::Seq),
    Approve(protocol::CallId, bool),
    /// Opens session `n` of the picker.
    Pick(usize),
    NewSession,
    /// Closes the overlay, or hides the recap.
    Dismiss,
    /// Scrolls back to the latest output.
    Bottom,
    Restart,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub action: Option<Action>,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            action: None,
        }
    }

    pub fn button(text: impl Into<String>, style: Style, action: Action) -> Self {
        Self {
            text: text.into(),
            style,
            action: Some(action),
        }
    }
}

/// One row on screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn plain(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![Span::new(text, style)],
        }
    }

    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| width_of(&s.text)).sum()
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

impl From<Vec<Span>> for Line {
    fn from(spans: Vec<Span>) -> Self {
        Self { spans }
    }
}

/// Columns `ch` takes; control characters take none (they are stripped).
pub fn char_width(ch: char) -> u16 {
    // Width is at most 2, so the narrowing is lossless.
    #[allow(clippy::cast_possible_truncation)]
    let w = ch.width().unwrap_or(0) as u16;
    w
}

pub fn width_of(text: &str) -> usize {
    text.chars().map(|c| usize::from(char_width(c))).sum()
}

/// `text` without terminal escapes or control characters (a command's
/// colored output would otherwise drive the terminal), tabs as spaces.
pub fn sanitize(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(|c| c.is_control() && c != '\n') {
        return text.into();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            // CSI: parameters and intermediates, then one final byte.
            '\x1b' if chars.peek() == Some(&'[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ST.
            '\x1b' if chars.peek() == Some(&']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.into()
}

/// Wraps a logical line of spans to `width` columns at spaces (breaking
/// words longer than a row): the first row after the `first` prefix, the
/// rest after `rest`. Leading spaces are kept (indentation); spaces a wrap
/// lands on are dropped.
pub fn wrap(spans: &[Span], width: usize, first: &[Span], rest: &[Span]) -> Vec<Line> {
    let rest_w: usize = rest.iter().map(|s| width_of(&s.text)).sum();
    let mut lines = vec![Line {
        spans: first.to_vec(),
    }];
    let mut col: usize = first.iter().map(|s| width_of(&s.text)).sum();
    let mut start = col;
    let mut spaces: Vec<Span> = Vec::new();
    let mut word: Vec<Span> = Vec::new();
    let mut place = |lines: &mut Vec<Line>, spaces: &mut Vec<Span>, word: &mut Vec<Span>| {
        let ww: usize = word.iter().map(|s| width_of(&s.text)).sum();
        let at_start = col == start;
        if !at_start && col + spaces.len() + ww > width {
            lines.push(Line {
                spans: rest.to_vec(),
            });
            col = rest_w;
            start = col;
            spaces.clear();
        } else if at_start && lines.len() > 1 {
            spaces.clear();
        }
        for space in spaces.drain(..) {
            if col < width {
                push(lines.last_mut().unwrap(), &space, " ");
                col += 1;
            }
        }
        for piece in word.drain(..) {
            let mut run = String::new();
            for ch in piece.text.chars() {
                let cw = usize::from(char_width(ch));
                if col + cw > width && col > start {
                    push(lines.last_mut().unwrap(), &piece, &run);
                    run.clear();
                    lines.push(Line {
                        spans: rest.to_vec(),
                    });
                    col = rest_w;
                    start = col;
                }
                run.push(ch);
                col += cw;
            }
            push(lines.last_mut().unwrap(), &piece, &run);
        }
    };
    for span in spans {
        // Pieces of `span`, styled as it is (not a clone of its text).
        let piece = |text: String| Span {
            text,
            style: span.style,
            action: span.action,
        };
        let mut run = String::new();
        for ch in span.text.chars() {
            if ch == ' ' {
                if !run.is_empty() {
                    word.push(piece(std::mem::take(&mut run)));
                }
                if !word.is_empty() {
                    place(&mut lines, &mut spaces, &mut word);
                }
                spaces.push(piece(String::new()));
            } else {
                run.push(ch);
            }
        }
        if !run.is_empty() {
            word.push(piece(run));
        }
    }
    if !word.is_empty() {
        place(&mut lines, &mut spaces, &mut word);
    }
    lines
}

/// Appends `text` to `line`, merged into its last span when styled alike.
fn push(line: &mut Line, from: &Span, text: &str) {
    if text.is_empty() {
        return;
    }
    match line.spans.last_mut() {
        Some(last) if last.style == from.style && last.action == from.action => {
            last.text.push_str(text);
        }
        _ => line.spans.push(Span {
            text: text.to_owned(),
            style: from.style,
            action: from.action,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    #[test]
    fn wraps_at_spaces_by_display_width() {
        let spans = [Span::new("the quick brown fox jumps", Style::new())];
        let lines = wrap(&spans, 10, &[], &[]);
        assert_eq!(texts(&lines), ["the quick", "brown fox", "jumps"]);
        let lines = wrap(&[Span::new("日本語 日本語", Style::new())], 7, &[], &[]);
        assert_eq!(texts(&lines), ["日本語", "日本語"]);
    }

    #[test]
    fn breaks_words_longer_than_a_row_and_indents_the_rest() {
        let pre = [Span::new("> ", Style::new())];
        let pad = [Span::new("  ", Style::new())];
        let lines = wrap(&[Span::new("abcdefghij k", Style::new())], 6, &pre, &pad);
        assert_eq!(texts(&lines), ["> abcd", "  efgh", "  ij k"]);
    }

    #[test]
    fn keeps_styles_across_a_wrap() {
        let bold = Style::new().bold();
        let spans = [Span::new("aaa ", Style::new()), Span::new("bbb ccc", bold)];
        let lines = wrap(&spans, 7, &[], &[]);
        assert_eq!(texts(&lines), ["aaa bbb", "ccc"]);
        assert_eq!(lines[1].spans[0].style, bold);
    }

    #[test]
    fn sanitize_strips_escapes_and_controls() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m\tx\x07"), "red    x");
        assert_eq!(sanitize("\x1b]0;title\x07ok"), "ok");
        assert!(matches!(
            sanitize("plain\ntext"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
