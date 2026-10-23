use crate::Skill;
use std::path::Path;

mod yaml;
use yaml::{Error, Kind, header};

/// Discovery reads only top-level identity lines. Invalid descriptions or
/// unrelated metadata in a losing file cannot reject a winning skill.
pub(crate) fn identity(source: &[u8]) -> Result<String, Error> {
    let mut lines = source.split(|b| *b == b'\n');
    if lines
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .is_none_or(|line| line.trim_end() != "---")
    {
        return Err(Error::new(1, Kind::Delimiter));
    }
    let mut name = None;
    for (index, line) in lines.enumerate() {
        if std::str::from_utf8(line).is_ok_and(|line| line.trim_end() == "---") {
            break;
        }
        let Some(rest) = line.strip_prefix(b"name:") else {
            continue;
        };
        if name.is_some() {
            return Err(Error::new(index + 2, Kind::Duplicate));
        }
        let rest = std::str::from_utf8(rest).map_err(|_| Error::new(index + 2, Kind::Syntax))?;
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return Err(Error::new(index + 2, Kind::Syntax));
        }
        let value = yaml::scalar(yaml::comment(rest.trim_start()).trim_end(), index + 2)?;
        validate_name(&value, index + 2)?;
        name = Some(value);
    }
    name.ok_or_else(|| Error::new(2, Kind::Missing))
}

fn validate_name(name: &str, line: usize) -> Result<(), Error> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return Err(Error::new(line, Kind::Name));
    }
    Ok(())
}

pub(crate) fn parse(source: &str, directory: &Path) -> Result<(String, Skill), Error> {
    let mut lines = source.split_inclusive('\n');
    let opening = lines.next().ok_or_else(|| Error::new(1, Kind::Delimiter))?;
    if opening.trim_end() != "---" {
        return Err(Error::new(1, Kind::Delimiter));
    }
    let start = opening.len();
    let mut end = start;
    for line in lines {
        if line.trim_end() == "---" {
            let header = header(&source[start..end])?;
            validate_name(&header.name, header.name_line)?;
            let description = header
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if description.is_empty() || description.len() > 1024 {
                return Err(Error::new(header.description_line, Kind::Description));
            }
            return Ok((
                header.name,
                Skill {
                    description,
                    directory: directory.display().to_string(),
                    body: source[end + line.len()..].to_owned(),
                },
            ));
        }
        end += line.len();
        if end - start > 8192 {
            return Err(Error::new(2, Kind::Size));
        }
    }
    Err(Error::new(source.lines().count(), Kind::Delimiter))
}

#[cfg(test)]
mod tests;
