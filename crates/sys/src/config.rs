//! `$A_HOME/config.toml`: the default model and agent, and the provider
//! variables a model call falls back to when its session has none.
//!
//! ```toml
//! model = "openai:gpt-6-luna"   # host[@protocol]:model
//! agent = "chat"                # "chat" (default) | "echo"
//!
//! [env]
//! OPENAI_API_KEY = "sk-..."
//! OPENAI_BASE_URL = "https://.../v1"
//! ```
//!
//! Only the TOML this schema needs is read: comments, `[table]` headers and
//! `key = "string"` (basic or literal). Anything else is kept verbatim on
//! write but ignored, so a hand-edited file loses nothing to the app's
//! settings pane. The file may hold secrets: it is written 0600, through an
//! exclusive temporary file renamed into place.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::Path;
use std::str::FromStr;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// `host[@protocol]:model`.
    pub model: Option<String>,
    pub agent: Option<String>,
    /// `[env]`: provider variables, such as `OPENAI_API_KEY`.
    pub env: BTreeMap<String, String>,
}

/// The keys this schema knows, by table (`None` is the root).
fn known(table: Option<&str>, key: &str) -> bool {
    match table {
        None => matches!(key, "model" | "agent"),
        Some(table) => table == "env",
    }
}

impl Config {
    /// The file at `path`; a missing file is the empty config.
    pub fn read(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => text.parse().map_err(|e: ParseError| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Writes this config to `path`, keeping what the file holds beyond the
    /// schema (comments, other keys and tables) where it was.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        for name in self.env.keys() {
            if !is_var_name(name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("not a variable name: {name:?}"),
                ));
            }
        }
        let old = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        write_private(path, self.render(&old).as_bytes())
    }

    /// `old` with the schema's keys replaced by this config's: in place
    /// where they were, else at the end of their table.
    fn render(&self, old: &str) -> String {
        let mut out = String::new();
        let mut table: Option<String> = None;
        let mut env_seen = false;
        let mut written = Vec::new();
        for line in old.lines() {
            match parse_line(line) {
                Ok(Line::Table(name)) => {
                    self.finish(table.as_deref(), &mut written, &mut out);
                    env_seen |= name == "env";
                    table = Some(name);
                }
                Ok(Line::Pair(key, _)) if known(table.as_deref(), &key) => {
                    let scoped = (table.is_some(), key);
                    if let Some(value) = self.value(table.as_deref(), &scoped.1)
                        && !written.contains(&scoped)
                    {
                        pair(&mut out, &scoped.1, value);
                        written.push(scoped);
                    }
                    continue;
                }
                _ => {}
            }
            out.push_str(line);
            out.push('\n');
        }
        self.finish(table.as_deref(), &mut written, &mut out);
        if !env_seen && !self.env.is_empty() {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str("[env]\n");
            self.finish(Some("env"), &mut written, &mut out);
        }
        out
    }

    /// Appends the keys of `table` not written yet, before the blank lines
    /// that end it. Root keys go before the first header: after it they
    /// would belong to that table.
    fn finish(&self, table: Option<&str>, written: &mut Vec<(bool, String)>, out: &mut String) {
        let missing: Vec<(&str, &str)> = match table {
            None => [("model", &self.model), ("agent", &self.agent)]
                .into_iter()
                .filter_map(|(k, v)| Some((k, v.as_deref()?)))
                .collect(),
            Some("env") => self
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect(),
            Some(_) => return,
        };
        let mut tail = String::new();
        for (key, value) in missing {
            let scoped = (table.is_some(), key.to_owned());
            if !written.contains(&scoped) {
                pair(&mut tail, key, value);
                written.push(scoped);
            }
        }
        if !tail.is_empty() {
            let blank = (out.len() - out.trim_end_matches('\n').len()).saturating_sub(1);
            out.truncate(out.len() - blank);
            out.push_str(&tail);
            out.push_str(&"\n".repeat(blank));
        }
    }

    fn value(&self, table: Option<&str>, key: &str) -> Option<&str> {
        match (table, key) {
            (None, "model") => self.model.as_deref(),
            (None, "agent") => self.agent.as_deref(),
            (None, _) => None,
            (Some(_), key) => self.env.get(key).map(String::as_str),
        }
    }
}

impl FromStr for Config {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        let mut config = Self::default();
        let mut table: Option<String> = None;
        for (i, line) in text.lines().enumerate() {
            let at = |message: String| ParseError {
                line: i + 1,
                message,
            };
            match parse_line(line).map_err(at)? {
                Line::Table(name) => table = Some(name),
                Line::Pair(key, value) if known(table.as_deref(), &key) => {
                    let value = value.ok_or_else(|| at(format!("{key} must be a string")))?;
                    match table {
                        None if key == "model" => config.model = Some(value),
                        None => config.agent = Some(value),
                        Some(_) => {
                            config.env.insert(key, value);
                        }
                    }
                }
                Line::Pair(..) | Line::Other => {}
            }
        }
        Ok(config)
    }
}

/// Where and why the file did not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

enum Line {
    Table(String),
    /// A key and its value, if that is a string.
    Pair(String, Option<String>),
    /// Blank, a comment, or something this schema does not read.
    Other,
}

fn parse_line(line: &str) -> Result<Line, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(Line::Other);
    }
    if let Some(rest) = line.strip_prefix('[') {
        if rest.starts_with('[') {
            return Ok(Line::Other);
        }
        let (name, after) = rest
            .split_once(']')
            .ok_or_else(|| "unclosed table header".to_owned())?;
        if !after.trim().is_empty() && !after.trim().starts_with('#') {
            return Err("text after a table header".into());
        }
        return Ok(Line::Table(name.trim().to_owned()));
    }
    let (key, rest) = if let Some(quoted) = line.strip_prefix('"') {
        let (key, len) = basic(quoted)?;
        (key, quoted[len..].trim_start())
    } else {
        let end = line
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(line.len());
        (line[..end].to_owned(), line[end..].trim_start())
    };
    let Some(value) = rest.strip_prefix('=').map(str::trim_start) else {
        // A dotted key or anything else: not ours.
        return Ok(Line::Other);
    };
    if key.is_empty() {
        return Err("a key is missing".into());
    }
    let (value, rest) = if let Some(quoted) = value.strip_prefix('"') {
        let (value, len) = basic(quoted)?;
        (Some(value), &quoted[len..])
    } else if let Some(quoted) = value.strip_prefix('\'') {
        let end = quoted
            .find('\'')
            .ok_or_else(|| "unclosed string".to_owned())?;
        (Some(quoted[..end].to_owned()), &quoted[end + 1..])
    } else {
        (None, "")
    };
    let rest = rest.trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        return Err(format!("text after the value of {key}"));
    }
    Ok(Line::Pair(key, value))
}

/// A basic string's value, from just after its opening quote, and the bytes
/// it took including the closing quote.
fn basic(s: &str) -> Result<(String, usize), String> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Ok((out, i + 1)),
            '\\' => {
                let (_, e) = chars.next().ok_or("unclosed string")?;
                match e {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'u' | 'U' => {
                        let len = if e == 'u' { 4 } else { 8 };
                        let hex: String = chars.by_ref().take(len).map(|(_, c)| c).collect();
                        let c = u32::from_str_radix(&hex, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .ok_or_else(|| format!("bad escape \\{e}{hex}"))?;
                        out.push(c);
                    }
                    e => return Err(format!("bad escape \\{e}")),
                }
            }
            c => out.push(c),
        }
    }
    Err("unclosed string".into())
}

fn pair(out: &mut String, key: &str, value: &str) {
    let _ = write!(out, "{key} = \"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push_str("\"\n");
}

/// A name a shell could export: written as a bare key.
pub fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Replaces `path` with `bytes`, readable only by this user, so a reader
/// sees the old file or the new one, never a torn or world-readable one.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(model: Option<&str>, env: &[(&str, &str)]) -> Config {
        Config {
            model: model.map(str::to_owned),
            agent: None,
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn reads_the_schema_and_skips_the_rest() {
        let text = r#"
# defaults
model = "openai:gpt-6-luna"   # the model
agent = 'echo'
other = 3

[env]
OPENAI_API_KEY = "sk-\"x\"\né"
"QUOTED_KEY" = 'lit\eral'

[other]
model = "not this one"
"#;
        let config: Config = text.parse().unwrap();
        assert_eq!(config.model.as_deref(), Some("openai:gpt-6-luna"));
        assert_eq!(config.agent.as_deref(), Some("echo"));
        assert_eq!(config.env["OPENAI_API_KEY"], "sk-\"x\"\n\u{e9}");
        assert_eq!(config.env["QUOTED_KEY"], "lit\\eral");
        assert_eq!(config.env.len(), 2);

        let bad = "model = 3\n".parse::<Config>().unwrap_err();
        assert_eq!(bad.line, 1);
        assert!("model = \"open\n".parse::<Config>().is_err());
        assert_eq!("".parse::<Config>().unwrap(), Config::default());
    }

    #[test]
    fn writing_keeps_what_the_schema_does_not_know() {
        let old = "# mine\nmodel = \"a:b\"\nother = 3\n\n[env]\n# keys\nA = \"1\"\nB = \"2\"\n\n[x]\nk = 1\n";
        let new = config(Some("c:d"), &[("A", "one"), ("C", "3")]);
        let text = new.render(old);
        assert_eq!(
            text,
            "# mine\nmodel = \"c:d\"\nother = 3\n\n[env]\n# keys\nA = \"one\"\nC = \"3\"\n\n[x]\nk = 1\n"
        );
        assert_eq!(text.parse::<Config>().unwrap(), new);
        // Idempotent.
        assert_eq!(new.render(&text), text);
    }

    #[test]
    fn writing_from_nothing_and_removing_keys() {
        let new = config(Some("openai:m"), &[("K", "a\"b\\c\nd")]);
        let text = new.render("");
        assert_eq!(
            text,
            "model = \"openai:m\"\n\n[env]\nK = \"a\\\"b\\\\c\\nd\"\n"
        );
        assert_eq!(text.parse::<Config>().unwrap(), new);

        // A root key goes before the first table, never into it.
        let text = config(Some("m"), &[]).render("[env]\nK = \"v\"\n");
        assert_eq!(text, "model = \"m\"\n[env]\n");
        assert_eq!(
            Config::default().render("model = \"m\"\n[env]\nK = \"v\"\n"),
            "[env]\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_and_replaced_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("a-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        assert_eq!(Config::read(&path).unwrap(), Config::default());
        let new = config(Some("m"), &[("OPENAI_API_KEY", "sk")]);
        new.write(&path).unwrap();
        assert_eq!(Config::read(&path).unwrap(), new);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temporary left");
        assert!(config(None, &[("BAD NAME", "x")]).write(&path).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
