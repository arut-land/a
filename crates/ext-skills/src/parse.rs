use crate::Skill;
use std::path::Path;

mod yaml;
use yaml::{Error, Kind, header};

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
            if header.name.is_empty()
                || header.name.len() > 64
                || !header
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || header.name.starts_with('-')
                || header.name.ends_with('-')
            {
                return Err(Error::new(header.name_line, Kind::Name));
            }
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
