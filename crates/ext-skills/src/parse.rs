use crate::Skill;
use std::path::Path;

mod yaml;
use yaml::{Error, Kind, header};

/// Discovery parses only identity entries. Other header failures remain raw
/// observations until capture and debounce have validated the winning inputs.
pub(crate) fn identity(source: &[u8]) -> Result<String, Error> {
    let mut lines = source.split_inclusive(|b| *b == b'\n');
    let opening = lines.next().ok_or_else(|| Error::new(1, Kind::Delimiter))?;
    if !std::str::from_utf8(opening).is_ok_and(|line| line.trim_end() == "---") {
        return Err(Error::new(1, Kind::Delimiter));
    }
    let mut end = opening.len();
    for line in lines {
        if std::str::from_utf8(line).is_ok_and(|line| line.trim_end() == "---") {
            break;
        }
        end += line.len();
    }
    // Replacement characters cannot make a valid ASCII skill identity. Bad
    // bytes in unrelated metadata must not obscure an otherwise readable name.
    let header = String::from_utf8_lossy(&source[opening.len()..end]);
    let indent = header
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut offset = 0;
    for (index, line) in header.split_inclusive('\n').enumerate() {
        if line.len() - line.trim_start().len() == indent
            && let Ok(Some((name, line))) = yaml::identity(&header[offset..], true)
        {
            validate_name(&name, line + index)?;
            return Ok(name);
        }
        offset += line.len();
    }
    // Flow mappings can put the name after another entry on the same line.
    if let Ok(Some((name, line))) = yaml::identity(&header, false) {
        validate_name(&name, line)?;
        return Ok(name);
    }
    Err(Error::new(2, Kind::Missing))
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
