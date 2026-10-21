use std::path::Path;

use serde::Deserialize;

use crate::Skill;

#[derive(Deserialize)]
struct Header {
    name: String,
    description: String,
}

pub(crate) fn parse(source: &str, directory: &Path) -> Result<(String, Skill), String> {
    let mut lines = source.split_inclusive('\n');
    let opening = lines.next().ok_or("expected opening ---")?;
    if opening.trim_end() != "---" {
        return Err("expected opening ---".into());
    }
    let start = opening.len();
    let mut end = start;
    for line in lines {
        if line.trim_end() == "---" {
            let header: Header =
                serde_yaml_ng::from_str(&source[start..end]).map_err(|e| e.to_string())?;
            if header.name.is_empty()
                || header.name.len() > 64
                || !header
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || header.name.starts_with('-')
                || header.name.ends_with('-')
            {
                return Err(
                    "name must be 1..64 lower-case letters, digits or internal hyphens".into(),
                );
            }
            let description = header
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if description.is_empty() || description.len() > 1024 {
                return Err("description must be 1..1024 bytes".into());
            }
            return Ok((
                header.name,
                Skill {
                    description,
                    directory: directory.to_path_buf(),
                    body: source[end + line.len()..].to_owned(),
                },
            ));
        }
        end += line.len();
        if end - start > 8192 {
            return Err("frontmatter exceeds 8 KiB".into());
        }
    }
    Err("expected closing ---".into())
}
