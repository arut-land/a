//! Styled text: the pen (palette colors and attributes), spans, word
//! wrapping by display width.

use smallvec::SmallVec;
pub use smol_str::SmolStr;
use unicode_width::UnicodeWidthChar;

/// The terminal palette entries the client draws with: the default, 1–6
/// and bright black. Never RGB and never a background, so light and dark
/// themes both read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Reset,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    DarkGray,
}

impl Color {
    /// The SGR parameter that sets it as the foreground; a background is
    /// 10 more.
    pub const fn sgr(self) -> u16 {
        match self {
            Self::Reset => 39,
            Self::Red => 31,
            Self::Green => 32,
            Self::Yellow => 33,
            Self::Blue => 34,
            Self::Magenta => 35,
            Self::Cyan => 36,
            Self::DarkGray => 90,
        }
    }
}

/// Attributes, as a set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifier(u8);

impl Modifier {
    pub const NONE: Self = Self(0);
    pub const BOLD: Self = Self(1);
    pub const DIM: Self = Self(2);
    pub const ITALIC: Self = Self(4);
    pub const UNDERLINED: Self = Self(8);
    pub const REVERSED: Self = Self(16);

    /// Each attribute and the SGR parameter that sets it.
    pub const ALL: [(Self, u16); 5] = [
        (Self::BOLD, 1),
        (Self::DIM, 2),
        (Self::ITALIC, 3),
        (Self::UNDERLINED, 4),
        (Self::REVERSED, 7),
    ];

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// What `self` has that `other` lacks.
    pub const fn minus(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

/// How text is drawn: the pen the terminal is set to before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub mods: Modifier,
}

impl Style {
    pub const fn new() -> Self {
        Self {
            fg: Color::Reset,
            bg: Color::Reset,
            mods: Modifier::NONE,
        }
    }

    pub const fn fg(self, fg: Color) -> Self {
        Self { fg, ..self }
    }

    pub const fn add_modifier(self, m: Modifier) -> Self {
        Self {
            mods: Modifier(self.mods.0 | m.0),
            ..self
        }
    }

    pub const fn bold(self) -> Self {
        self.add_modifier(Modifier::BOLD)
    }

    pub const fn dim(self) -> Self {
        self.add_modifier(Modifier::DIM)
    }

    pub const fn italic(self) -> Self {
        self.add_modifier(Modifier::ITALIC)
    }

    pub const fn reversed(self) -> Self {
        self.add_modifier(Modifier::REVERSED)
    }
}

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

/// A run of text drawn with one pen. Its text is inline up to 23 bytes and
/// shared past that, so a span (and a row of them) clones without
/// allocating: a frame keeps the rows it drew before by value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: SmolStr,
    pub style: Style,
    pub action: Option<Action>,
}

impl Span {
    pub fn new(text: impl Into<SmolStr>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            action: None,
        }
    }

    pub fn button(text: impl Into<SmolStr>, style: Style, action: Action) -> Self {
        Self {
            text: text.into(),
            style,
            action: Some(action),
        }
    }

    /// The pen and the click, which decide whether two runs are one span.
    fn pen(&self) -> (Style, Option<Action>) {
        (self.style, self.action)
    }
}

/// One row on screen: most rows are a prefix and a run or two, kept inline.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: SmallVec<[Span; 4]>,
}

impl Line {
    pub fn plain(text: impl Into<SmolStr>, style: Style) -> Self {
        Self::from([Span::new(text, style)])
    }

    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| width_of(&s.text)).sum()
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

impl<const N: usize> From<[Span; N]> for Line {
    fn from(spans: [Span; N]) -> Self {
        Self {
            spans: SmallVec::from_iter(spans),
        }
    }
}

impl From<Vec<Span>> for Line {
    fn from(spans: Vec<Span>) -> Self {
        Self {
            spans: SmallVec::from_vec(spans),
        }
    }
}

impl From<&[Span]> for Line {
    fn from(spans: &[Span]) -> Self {
        Self {
            spans: spans.iter().cloned().collect(),
        }
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

/// Rows being wrapped. The open row's last run is gathered as it grows
/// and becomes a span when its pen changes or the row ends, so a row costs
/// one span per pen, not one per word.
struct Rows<'a> {
    lines: Vec<Line>,
    tail: String,
    pen: (Style, Option<Action>),
    rest: &'a [Span],
}

impl Rows<'_> {
    fn push(&mut self, from: &Span, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.pen != from.pen() {
            self.seal();
            self.pen = from.pen();
        }
        self.tail.push_str(text);
    }

    fn seal(&mut self) {
        if self.tail.is_empty() {
            return;
        }
        let (style, action) = self.pen;
        self.lines.last_mut().unwrap().spans.push(Span {
            text: SmolStr::new(&self.tail),
            style,
            action,
        });
        self.tail.clear();
    }

    /// Starts the next row after the `rest` prefix.
    fn newline(&mut self) {
        self.seal();
        self.lines.push(Line::from(self.rest));
    }
}

/// Wraps a logical line of spans to `width` columns at spaces (breaking
/// words longer than a row): the first row after the `first` prefix, the
/// rest after `rest`. Leading spaces are kept (indentation); spaces a wrap
/// lands on are dropped.
pub fn wrap(spans: &[Span], width: usize, first: &[Span], rest: &[Span]) -> Vec<Line> {
    let rest_w: usize = rest.iter().map(|s| width_of(&s.text)).sum();
    let mut out = Rows {
        lines: vec![Line::from(first)],
        tail: String::new(),
        pen: (Style::new(), None),
        rest,
    };
    let mut col: usize = first.iter().map(|s| width_of(&s.text)).sum();
    let mut start = col;
    // The spans the spaces before a word came from, and the word's pieces.
    let mut spaces: Vec<&Span> = Vec::new();
    let mut word: Vec<(&Span, &str)> = Vec::new();
    let mut place = |out: &mut Rows, spaces: &mut Vec<&Span>, word: &mut Vec<(&Span, &str)>| {
        let ww: usize = word.iter().map(|(_, text)| width_of(text)).sum();
        let at_start = col == start;
        if !at_start && col + spaces.len() + ww > width {
            out.newline();
            col = rest_w;
            start = col;
            spaces.clear();
        } else if at_start && out.lines.len() > 1 {
            spaces.clear();
        }
        for space in spaces.drain(..) {
            if col < width {
                out.push(space, " ");
                col += 1;
            }
        }
        for (span, text) in word.drain(..) {
            let mut from = 0;
            for (i, ch) in text.char_indices() {
                let cw = usize::from(char_width(ch));
                if col + cw > width && col > start {
                    out.push(span, &text[from..i]);
                    from = i;
                    out.newline();
                    col = rest_w;
                    start = col;
                }
                col += cw;
            }
            out.push(span, &text[from..]);
        }
    };
    for span in spans {
        let text = span.text.as_str();
        let mut from = 0;
        for (i, ch) in text.char_indices() {
            if ch != ' ' {
                continue;
            }
            if i > from {
                word.push((span, &text[from..i]));
            }
            if !word.is_empty() {
                place(&mut out, &mut spaces, &mut word);
            }
            spaces.push(span);
            from = i + 1;
        }
        if from < text.len() {
            word.push((span, &text[from..]));
        }
    }
    if !word.is_empty() {
        place(&mut out, &mut spaces, &mut word);
    }
    out.seal();
    out.lines
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

    /// Spans are per pen, not per word: a plain paragraph wraps to one
    /// span a row (after the prefix), whatever its length.
    #[test]
    fn a_row_holds_one_span_per_pen() {
        let text = ["word"; 40].join(" ");
        let pad = [Span::new("  ", MUTED)];
        let lines = wrap(&[Span::new(text, Style::new())], 30, &pad, &pad);
        assert!(lines.len() > 5);
        for line in &lines {
            assert_eq!(line.spans.len(), 2, "{line:?}");
            assert_eq!(line.spans[0].text, "  ");
        }
        // Leading spaces (indentation) stay; a space a wrap lands on goes.
        let lines = wrap(&[Span::new("  in dented", Style::new())], 5, &[], &[]);
        assert_eq!(texts(&lines), ["  in", "dente", "d"]);
    }

    const MUTED: Style = Style::new().fg(Color::DarkGray);

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
