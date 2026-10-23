//! Bounded skill headers, without YAML's tags, aliases or implicit typing.
use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Delimiter,
    Size,
    Depth,
    Syntax,
    Unsupported,
    Duplicate,
    Missing,
    Collection,
    Name,
    Description,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Error {
    pub line: usize,
    pub kind: Kind,
}
impl Error {
    pub(super) fn new(line: usize, kind: Kind) -> Self {
        Self { line, kind }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {:?}", self.line, self.kind)
    }
}
impl std::error::Error for Error {}

pub(super) struct Header {
    pub name: String,
    pub description: String,
    pub name_line: usize,
    pub description_line: usize,
}
struct Parser<'a> {
    lines: Vec<&'a str>,
    pos: usize,
}

pub(super) fn header(source: &str) -> Result<Header, Error> {
    if source.len() > 8192 {
        return Err(Error::new(2, Kind::Size));
    }
    if source
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        let line = source
            .lines()
            .position(|line| line.chars().any(|c| c.is_control() && c != '\t'))
            .unwrap_or(0)
            + 2;
        return Err(Error::new(line, Kind::Syntax));
    }
    let mut p = Parser {
        lines: source.lines().collect(),
        pos: 0,
    };
    let mut name = None;
    let mut description = None;
    let mut name_line = 2;
    let mut description_line = 2;
    p.mapping(0, 0, &mut |key, value, line| {
        let slot = match key {
            "name" => {
                name_line = line;
                &mut name
            }
            "description" => {
                description_line = line;
                &mut description
            }
            _ => return Ok(()),
        };
        if slot.is_some() {
            return Err(Error::new(line, Kind::Duplicate));
        }
        *slot = Some(value.ok_or_else(|| Error::new(line, Kind::Collection))?);
        Ok(())
    })?;
    Ok(Header {
        name_line,
        description_line,
        name: name.ok_or_else(|| Error::new(2, Kind::Missing))?,
        description: description.ok_or_else(|| Error::new(2, Kind::Missing))?,
    })
}

type Field<'a> = dyn FnMut(&str, Option<String>, usize) -> Result<(), Error> + 'a;

impl Parser<'_> {
    fn error(&self, kind: Kind) -> Error {
        Error::new(self.pos + 2, kind)
    }
    fn next(&mut self) -> Result<Option<(usize, &str)>, Error> {
        while let Some(raw) = self.lines.get(self.pos).copied() {
            let text = raw.trim_start_matches(' ');
            if text.starts_with('\t') || raw.chars().any(|c| c.is_control() && c != '\t') {
                return Err(self.error(Kind::Syntax));
            }
            if text.is_empty() || text.starts_with('#') {
                self.pos += 1;
                continue;
            }
            return Ok(Some((raw.len() - text.len(), text)));
        }
        Ok(None)
    }
    fn mapping(&mut self, indent: usize, depth: usize, field: &mut Field<'_>) -> Result<(), Error> {
        if depth > 16 {
            return Err(self.error(Kind::Depth));
        }
        let mut keys = Vec::new();
        while let Some((spaces, text)) = self.next()? {
            if spaces < indent {
                break;
            }
            if spaces != indent {
                return Err(self.error(Kind::Syntax));
            }
            let text = text.to_owned();
            let (key, rest) = text
                .split_once(':')
                .ok_or_else(|| self.error(Kind::Syntax))?;
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || (!rest.is_empty() && !rest.starts_with(char::is_whitespace))
            {
                return Err(self.error(Kind::Syntax));
            }
            if keys.contains(&key.to_owned()) {
                return Err(self.error(Kind::Duplicate));
            }
            keys.push(key.to_owned());
            let line = self.pos + 2;
            self.pos += 1;
            let value = self.value(rest.trim_start(), indent, depth + 1)?;
            field(key, value, line)?;
        }
        Ok(())
    }
    fn value(&mut self, text: &str, indent: usize, depth: usize) -> Result<Option<String>, Error> {
        if depth > 16 {
            return Err(self.error(Kind::Depth));
        }
        let text = comment(text).trim_end();
        if matches!(text, "null" | "Null" | "NULL" | "~") {
            return Ok(None);
        }
        if text.is_empty() {
            if let Some((spaces, next)) = self.next()?
                && spaces > indent
            {
                if next == "-" || next.starts_with("- ") {
                    self.sequence(spaces, depth)?;
                } else {
                    self.mapping(spaces, depth, &mut |_, _, _| Ok(()))?;
                }
            }
            return Ok(None);
        }
        if text.starts_with(['|', '>']) {
            return self.block(text, indent).map(Some);
        }
        if text.starts_with(['[', '{']) {
            let mut flow = Flow {
                text,
                pos: 0,
                line: self.pos + 1,
            };
            flow.value(depth)?;
            flow.space();
            if flow.pos != text.len() {
                return Err(Error::new(self.pos + 1, Kind::Syntax));
            }
            return Ok(None);
        }
        scalar(text, self.pos + 1).map(Some)
    }
    fn sequence(&mut self, indent: usize, depth: usize) -> Result<(), Error> {
        while let Some((spaces, text)) = self.next()? {
            if spaces < indent {
                break;
            }
            let text = text.to_owned();
            if spaces != indent || !(text == "-" || text.starts_with("- ")) {
                return Err(self.error(Kind::Syntax));
            }
            self.pos += 1;
            self.value(text[1..].trim_start(), indent, depth + 1)?;
        }
        Ok(())
    }
    fn block(&mut self, marker: &str, parent: usize) -> Result<String, Error> {
        let marker_line = self.pos + 1;
        let options = &marker[1..];
        let mut explicit = None;
        let mut chomp = None;
        for c in options.chars() {
            match c {
                '1'..='9' if explicit.is_none() => explicit = c.to_digit(10).map(|n| n as usize),
                '+' | '-' if chomp.is_none() => chomp = Some(c),
                _ => return Err(Error::new(marker_line, Kind::Syntax)),
            }
        }
        let mut indent = explicit.map(|n| parent + n);
        let mut result = String::new();
        while let Some(raw) = self.lines.get(self.pos).copied() {
            let spaces = raw.bytes().take_while(|&b| b == b' ').count();
            if !raw.trim().is_empty() {
                if spaces <= parent {
                    break;
                }
                let required = *indent.get_or_insert(spaces);
                if spaces < required {
                    return Err(self.error(Kind::Syntax));
                }
            }
            // Descriptions fold all whitespace at the extension boundary anyway.
            let content = raw.get(indent.unwrap_or(spaces)..).unwrap_or("");
            result.push_str(content);
            result.push('\n');
            self.pos += 1;
        }
        Ok(result)
    }
}

fn comment(s: &str) -> &str {
    if !s.starts_with(['\'', '"', '[', '{']) {
        return s
            .char_indices()
            .find(|(i, c)| *c == '#' && (*i == 0 || s[..*i].ends_with(char::is_whitespace)))
            .map_or(s, |(i, _)| &s[..i]);
    }
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && c == '\\' {
            escaped = true;
            continue;
        }
        if Some(c) == quote {
            quote = None;
        } else if quote.is_none() {
            if (c == '\'' || c == '"')
                && (i == 0 || s[..i].trim_end().ends_with(['[', '{', ',', ':']))
            {
                quote = Some(c);
            }
            if c == '#' && (i == 0 || s[..i].ends_with(char::is_whitespace)) {
                return &s[..i];
            }
        }
    }
    s
}
fn scalar(text: &str, line: usize) -> Result<String, Error> {
    let err = || Error::new(line, Kind::Syntax);
    if text.starts_with(['&', '*', '!', '%', '@', '`']) {
        return Err(Error::new(line, Kind::Unsupported));
    }
    if let Some(inner) = text.strip_prefix('\'') {
        let inner = inner.strip_suffix('\'').ok_or_else(err)?;
        let mut chars = inner.chars();
        let mut out = String::new();
        while let Some(c) = chars.next() {
            if c == '\'' && chars.next() != Some('\'') {
                return Err(err());
            }
            out.push(c);
        }
        return Ok(out);
    }
    if text.starts_with('"') {
        // JSON's quoted strings are a strict, common subset of YAML escapes.
        return serde_json::from_str::<String>(text).map_err(|_| err());
    }
    if text.contains(": ")
        || text.ends_with(':')
        || text.starts_with(['[', '{', ']', '}', '|', '>'])
        || text == "---"
        || text == "..."
    {
        return Err(err());
    }
    Ok(text.to_owned())
}

struct Flow<'a> {
    text: &'a str,
    pos: usize,
    line: usize,
}
impl Flow<'_> {
    fn space(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }
    fn value(&mut self, depth: usize) -> Result<(), Error> {
        if depth > 16 {
            return Err(Error::new(self.line, Kind::Depth));
        }
        self.space();
        let start = self.pos;
        let first = *self
            .text
            .as_bytes()
            .get(self.pos)
            .ok_or_else(|| Error::new(self.line, Kind::Syntax))?;
        if first == b'[' || first == b'{' {
            self.pos += 1;
            let close = if first == b'[' { b']' } else { b'}' };
            self.space();
            if self.text.as_bytes().get(self.pos) == Some(&close) {
                self.pos += 1;
                return Ok(());
            }
            loop {
                if first == b'{' {
                    self.atom(true)?;
                    self.space();
                    if self.text.as_bytes().get(self.pos) != Some(&b':') {
                        return Err(Error::new(self.line, Kind::Syntax));
                    }
                    self.pos += 1;
                }
                self.value(depth + 1)?;
                self.space();
                match self.text.as_bytes().get(self.pos) {
                    Some(b',') => {
                        self.pos += 1;
                        self.space();
                        if self.text.as_bytes().get(self.pos) == Some(&close) {
                            self.pos += 1;
                            break;
                        }
                    }
                    Some(b) if *b == close => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(Error::new(self.line, Kind::Syntax)),
                }
            }
        } else {
            self.atom(false)?;
        }
        if self.pos == start {
            return Err(Error::new(self.line, Kind::Syntax));
        }
        Ok(())
    }
    fn atom(&mut self, key: bool) -> Result<(), Error> {
        self.space();
        let start = self.pos;
        let quote = self
            .text
            .as_bytes()
            .get(start)
            .copied()
            .filter(|b| *b == b'\'' || *b == b'"');
        if let Some(q) = quote {
            self.pos += 1;
            loop {
                let b = *self
                    .text
                    .as_bytes()
                    .get(self.pos)
                    .ok_or_else(|| Error::new(self.line, Kind::Syntax))?;
                self.pos += 1;
                if b == b'\\' && q == b'"' {
                    self.pos += 1;
                } else if b == q {
                    if q == b'\'' && self.text.as_bytes().get(self.pos) == Some(&q) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
        } else {
            while let Some(b) = self.text.as_bytes().get(self.pos) {
                if b",]}".contains(b) || (key && *b == b':') {
                    break;
                }
                self.pos += 1;
            }
        }
        let text = self
            .text
            .get(start..self.pos)
            .ok_or_else(|| Error::new(self.line, Kind::Syntax))?
            .trim();
        if text.is_empty() {
            return Err(Error::new(self.line, Kind::Syntax));
        }
        scalar(text, self.line)?;
        Ok(())
    }
}
