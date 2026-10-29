//! The markdown a reply gets, read as blocks and inline runs that borrow
//! the text: headings, bullets, quotes, fenced code, `**bold**` and
//! `` `code` ``. Each renderer lays them out; none parses.

/// Parser state at a requested source offset; it owns no transcript text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    pub fenced: bool,
    pub inline: Option<InlineStyle>,
    pub block: Option<LineKind>,
    pub skip: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InlineStyle {
    pub bold: bool,
    pub code: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LineKind {
    Heading(u8),
    Bullet(u8),
    Quote,
    Fence,
    Code,
    Para,
}

impl Checkpoint {
    pub fn at(text: &str, offset: usize) -> Self {
        let prefix = &text[..offset];
        let line_start = prefix.rfind('\n').map_or(0, |at| at + 1);
        let mut preceding = blocks(&prefix[..line_start]);
        while preceding.next().is_some() {}
        let mut inline = inlines(&prefix[line_start..]);
        while inline.next().is_some() {}
        let line_end = text[offset..]
            .find('\n')
            .map_or(text.len(), |at| offset + at);
        let line = &text[line_start..line_end];
        let (block, prefix) = match blocks(line).fenced(preceding.in_fence()).next().unwrap() {
            Block::Heading(level, body) => (LineKind::Heading(level), line.len() - body.len()),
            Block::Bullet { indent, text: body } => {
                (LineKind::Bullet(indent), line.len() - body.len())
            }
            Block::Quote(body) => (LineKind::Quote, line.len() - body.len()),
            Block::Fence(_) => (LineKind::Fence, line.len()),
            Block::Code(_) => (LineKind::Code, 0),
            Block::Para(_) => (LineKind::Para, 0),
        };
        Self {
            fenced: preceding.in_fence(),
            inline: (line_start != offset).then_some(InlineStyle {
                bold: inline.bold,
                code: inline.code,
            }),
            block: Some(block),
            skip: prefix.saturating_sub(offset - line_start),
        }
    }
}

/// One line of a reply, by what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block<'a> {
    /// `#` to `######`, the level and the text after them.
    Heading(u8, &'a str),
    /// `- ` or `* `, with the spaces before it.
    Bullet {
        indent: u8,
        text: &'a str,
    },
    /// `> `.
    Quote(&'a str),
    /// A ```` ``` ```` line, opening or closing a fence, as written.
    Fence(&'a str),
    /// A line inside a fence, verbatim.
    Code(&'a str),
    Para(&'a str),
}

/// A run within a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline<'a> {
    Text(&'a str),
    Bold(&'a str),
    Code(&'a str),
}

/// Inline runs for a text continuation, carrying fence and first-line state.
pub fn fragment(text: &str, checkpoint: Checkpoint) -> Vec<Inline<'_>> {
    let mut runs = Vec::new();
    let mut fenced = checkpoint.fenced;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let (kind, body) = if index == 0
            && let Some(kind) = checkpoint.block
        {
            (kind, &content[checkpoint.skip.min(content.len())..])
        } else {
            match blocks(content).fenced(fenced).next().unwrap() {
                Block::Heading(level, body) => (LineKind::Heading(level), body),
                Block::Bullet { indent, text } => (LineKind::Bullet(indent), text),
                Block::Quote(body) => (LineKind::Quote, body),
                Block::Fence(_) => (LineKind::Fence, ""),
                Block::Code(body) => (LineKind::Code, body),
                Block::Para(body) => (LineKind::Para, body),
            }
        };
        match kind {
            LineKind::Fence => fenced = !fenced,
            LineKind::Code => runs.push(Inline::Code(body)),
            _ => {
                if matches!(kind, LineKind::Bullet(_))
                    && (index != 0 || checkpoint.inline.is_none())
                {
                    runs.push(Inline::Text("• "));
                }
                let state = if index == 0 {
                    checkpoint
                } else {
                    Checkpoint::default()
                };
                runs.extend(inlines(body).resumed(state).map(|run| match run {
                    Inline::Text(text) if matches!(kind, LineKind::Heading(_)) => {
                        Inline::Bold(text)
                    }
                    run => run,
                }));
            }
        }
        if let Some(newline) = line.get(content.len()..)
            && !newline.is_empty()
        {
            runs.push(Inline::Text(newline));
        }
    }
    runs
}

/// The lines of `text` as blocks.
pub fn blocks(text: &str) -> Blocks<'_> {
    Blocks {
        lines: text.split('\n'),
        fenced: false,
    }
}

/// The runs of one line.
pub fn inlines(line: &str) -> Inlines<'_> {
    Inlines {
        rest: line,
        bold: false,
        code: false,
    }
}

/// [`blocks`]: one per line, a fence's state carried from line to line.
pub struct Blocks<'a> {
    lines: std::str::Split<'a, char>,
    fenced: bool,
}

impl Blocks<'_> {
    /// Starts inside a fence when `inside`: for text that follows some
    /// already read.
    #[must_use]
    pub const fn fenced(mut self, inside: bool) -> Self {
        self.fenced = inside;
        self
    }

    /// Whether the lines read so far end inside a fence.
    pub const fn in_fence(&self) -> bool {
        self.fenced
    }
}

impl<'a> Iterator for Blocks<'a> {
    type Item = Block<'a>;

    fn next(&mut self) -> Option<Block<'a>> {
        let raw = self.lines.next()?;
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            self.fenced = !self.fenced;
            return Some(Block::Fence(trimmed));
        }
        if self.fenced {
            return Some(Block::Code(raw));
        }
        Some(if let Some((level, text)) = heading(trimmed) {
            Block::Heading(level, text)
        } else if let Some(text) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let indent = u8::try_from(raw.len() - trimmed.len()).unwrap_or(u8::MAX);
            Block::Bullet { indent, text }
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            Block::Quote(quote.trim_start())
        } else {
            Block::Para(raw)
        })
    }
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    let text = (1..=6)
        .contains(&hashes)
        .then(|| line[hashes..].strip_prefix(' '))
        .flatten()?;
    Some((u8::try_from(hashes).unwrap_or(6), text))
}

/// [`inlines`].
pub struct Inlines<'a> {
    rest: &'a str,
    bold: bool,
    code: bool,
}

impl Inlines<'_> {
    #[must_use]
    pub fn resumed(mut self, checkpoint: Checkpoint) -> Self {
        self.bold = checkpoint.inline.is_some_and(|style| style.bold);
        self.code = checkpoint.inline.is_some_and(|style| style.code);
        self
    }
}

impl<'a> Iterator for Inlines<'a> {
    type Item = Inline<'a>;

    fn next(&mut self) -> Option<Inline<'a>> {
        loop {
            if self.rest.is_empty() {
                return None;
            }
            let next = if self.code {
                self.rest.find('`').map(|i| (i, 1))
            } else {
                match (self.rest.find("**"), self.rest.find('`')) {
                    (Some(b), Some(c)) if c < b => Some((c, 1)),
                    (Some(b), _) => Some((b, 2)),
                    (None, Some(c)) => Some((c, 1)),
                    (None, None) => None,
                }
            };
            let run = match (self.code, self.bold) {
                (true, _) => Inline::Code,
                (false, true) => Inline::Bold,
                (false, false) => Inline::Text,
            };
            let Some((at, len)) = next else {
                return Some(run(std::mem::take(&mut self.rest)));
            };
            let (text, rest) = self.rest.split_at(at);
            self.rest = &rest[len..];
            if len == 1 {
                self.code = !self.code;
            } else {
                self.bold = !self.bold;
            }
            // The run before the marker, in the style before it.
            if !text.is_empty() {
                return Some(run(text));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_borrow_their_lines_and_keep_the_fence() {
        let text = "# Title\n- one **two** `x`\n  * nested\n> said\n```\nfn a()\n```\nend";
        let got: Vec<_> = blocks(text).collect();
        assert_eq!(
            got,
            [
                Block::Heading(1, "Title"),
                Block::Bullet {
                    indent: 0,
                    text: "one **two** `x`"
                },
                Block::Bullet {
                    indent: 2,
                    text: "nested"
                },
                Block::Quote("said"),
                Block::Fence("```"),
                Block::Code("fn a()"),
                Block::Fence("```"),
                Block::Para("end"),
            ]
        );
        let mut open = blocks("```\ncode");
        assert_eq!(open.by_ref().count(), 2);
        assert!(open.in_fence());
        assert_eq!(
            blocks("more").fenced(true).collect::<Vec<_>>(),
            [Block::Code("more")]
        );
    }

    #[test]
    fn inlines_split_bold_and_code() {
        let got: Vec<_> = inlines("a **b** `c **d**` e").collect();
        assert_eq!(
            got,
            [
                Inline::Text("a "),
                Inline::Bold("b"),
                Inline::Text(" "),
                Inline::Code("c **d**"),
                Inline::Text(" e"),
            ]
        );
        assert_eq!(
            inlines("**open").collect::<Vec<_>>(),
            [Inline::Bold("open")]
        );
        assert_eq!(inlines("").count(), 0);
    }
}
