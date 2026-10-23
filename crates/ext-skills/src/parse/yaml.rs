//! Bounded skill metadata over maintained YAML syntax parsing.
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
struct HeaderParser<'a> {
    parser: yaml_rust2::parser::Parser<std::str::Chars<'a>>,
    name: Option<(String, usize)>,
    description: Option<(String, usize)>,
}

pub(super) fn header(source: &str) -> Result<Header, Error> {
    if source.len() > 8192 {
        return Err(Error::new(2, Kind::Size));
    }
    if let Some((line, _)) = source.lines().enumerate().find(|(_, text)| {
        text.chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\r'))
    }) {
        return Err(Error::new(line + 2, Kind::Syntax));
    }
    let mut p = HeaderParser {
        parser: yaml_rust2::parser::Parser::new_from_str(source),
        name: None,
        description: None,
    };
    p.expect(yaml_rust2::parser::Event::StreamStart)?;
    p.expect(yaml_rust2::parser::Event::DocumentStart)?;
    let (event, line) = p.next()?;
    if !matches!(event, yaml_rust2::parser::Event::MappingStart(0, None)) {
        return Err(Error::new(line, Kind::Collection));
    }
    p.mapping(1)?;
    p.expect(yaml_rust2::parser::Event::DocumentEnd)?;
    p.expect(yaml_rust2::parser::Event::StreamEnd)?;
    let (name, name_line) = p.name.ok_or_else(|| Error::new(2, Kind::Missing))?;
    let (description, description_line) =
        p.description.ok_or_else(|| Error::new(2, Kind::Missing))?;
    Ok(Header {
        name,
        name_line,
        description,
        description_line,
    })
}

impl HeaderParser<'_> {
    fn next(&mut self) -> Result<(yaml_rust2::parser::Event, usize), Error> {
        self.parser
            .next_token()
            .map(|(event, mark)| (event, mark.line() + 1))
            .map_err(|e| Error::new(e.marker().line() + 1, Kind::Syntax))
    }

    fn expect(&mut self, expected: yaml_rust2::parser::Event) -> Result<(), Error> {
        let (event, line) = self.next()?;
        if event == expected {
            Ok(())
        } else {
            Err(Error::new(line, Kind::Syntax))
        }
    }

    fn mapping(&mut self, depth: usize) -> Result<(), Error> {
        use yaml_rust2::parser::Event;
        let mut keys = std::collections::BTreeSet::new();
        loop {
            let (event, line) = self.next()?;
            if event == Event::MappingEnd {
                return Ok(());
            }
            let key =
                Self::scalar(event, line)?.ok_or_else(|| Error::new(line, Kind::Collection))?;
            if !keys.insert(key.clone()) {
                return Err(Error::new(line, Kind::Duplicate));
            }
            let (event, value_line) = self.next()?;
            let value = self.value(event, value_line, depth + 1)?;
            if depth == 1 {
                let slot = match key.as_str() {
                    "name" => &mut self.name,
                    "description" => &mut self.description,
                    _ => continue,
                };
                *slot = Some((
                    value.ok_or_else(|| Error::new(value_line, Kind::Collection))?,
                    value_line,
                ));
            }
        }
    }

    fn value(
        &mut self,
        event: yaml_rust2::parser::Event,
        line: usize,
        depth: usize,
    ) -> Result<Option<String>, Error> {
        use yaml_rust2::parser::Event;
        if depth > 16 {
            return Err(Error::new(line, Kind::Depth));
        }
        match event {
            Event::MappingStart(0, None) => {
                self.mapping(depth)?;
                Ok(None)
            }
            Event::SequenceStart(0, None) => {
                loop {
                    let (event, line) = self.next()?;
                    if event == Event::SequenceEnd {
                        break;
                    }
                    self.value(event, line, depth + 1)?;
                }
                Ok(None)
            }
            event => Self::scalar(event, line),
        }
    }

    fn scalar(event: yaml_rust2::parser::Event, line: usize) -> Result<Option<String>, Error> {
        use yaml_rust2::{parser::Event, scanner::TScalarStyle};
        match event {
            Event::Scalar(text, style, 0, None) => {
                if style == TScalarStyle::Plain
                    && matches!(text.as_str(), "" | "null" | "Null" | "NULL" | "~")
                {
                    Ok(None)
                } else {
                    Ok(Some(text))
                }
            }
            Event::Alias(_)
            | Event::Scalar(..)
            | Event::MappingStart(..)
            | Event::SequenceStart(..) => Err(Error::new(line, Kind::Unsupported)),
            _ => Err(Error::new(line, Kind::Syntax)),
        }
    }
}
