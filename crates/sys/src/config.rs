//! `$A_HOME/config.toml`: the default model and agent, and the provider
//! variables a model call falls back to when its session has none.
//!
//! ```toml
//! model = "openai:gpt-6-luna"   # host[@protocol]:model
//! agent = "loop"                # "loop" (default) | "echo"
//!
//! [env]
//! OPENAI_API_KEY = "sk-..."
//! OPENAI_BASE_URL = "https://.../v1"
//!
//! [daemon]                      # read when the daemon starts
//! idle_secs = 600               # exit after this long unused
//! hibernate_secs = 300          # unload a session after this long unused
//! drain_secs = 60               # how long an upgrade waits for work
//! agent = "echo"                # every session's agent, over both files
//! crash_limit = 3               # young deaths in a row before refusing
//!
//! [ext]                         # the extensions, read when the daemon starts
//! disable = ["subagent"]        # extensions left out, by name
//!
//! [ext.loop]                    # each extension's own table, by its name
//! max_steps = 20
//!
//! [ext.policy]
//! ask = ["bash", "edit"]        # tools whose calls are asked about
//! mode = "ask"                  # ask | accept-edits | auto | judge
//!
//! [ext.prompt]
//! system = "..."                # in place of the prompt's identity line
//! ```
//!
//! The file is edited in place: writing changes only the schema's keys, so
//! comments and the keys and tables a hand-edited file adds survive the
//! app's settings pane. The file may hold secrets: it is written 0600,
//! through an exclusive temporary file renamed into place.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::Path;
use std::str::FromStr;

use toml_edit::{DocumentMut, Item, TableLike, TomlError};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// `host[@protocol]:model`.
    pub model: Option<String>,
    pub agent: Option<String>,
    /// `[env]`: provider variables, such as `OPENAI_API_KEY`.
    pub env: BTreeMap<String, String>,
    /// `[daemon]`: how the daemon runs. Its own table, so the daemon never
    /// takes a setting from the environment of whichever client started it.
    pub daemon: Daemon,
    pub ext: Exts,
}

/// The `[ext]` table: the extensions left out, and each one's own table,
/// `[ext.<name>]`, which only that extension reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exts {
    pub disable: Vec<String>,
    pub tables: BTreeMap<String, Table>,
}

impl Exts {
    /// Extension `name`'s table; empty when the file has none.
    pub fn table(&self, name: &str) -> Table {
        self.tables.get(name).cloned().unwrap_or_else(|| Table {
            name: name.to_owned(),
            values: BTreeMap::new(),
        })
    }
}

/// One extension's `[ext.<name>]` table: its settings, of the few shapes
/// an extension takes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    name: String,
    values: BTreeMap<String, Setting>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Setting {
    Text(String),
    Whole(u64),
    Flag(bool),
    List(Vec<String>),
}

impl Table {
    /// The extension's name: the table is `[ext.<name>]`.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn text(&self, key: &str) -> Result<Option<&str>, ParseError> {
        self.get(key, "a string", |v| match v {
            Setting::Text(text) => Some(text.as_str()),
            _ => None,
        })
    }

    pub fn whole(&self, key: &str) -> Result<Option<u64>, ParseError> {
        self.get(key, "a whole number", |v| match v {
            Setting::Whole(n) => Some(*n),
            _ => None,
        })
    }

    pub fn flag(&self, key: &str) -> Result<Option<bool>, ParseError> {
        self.get(key, "true or false", |v| match v {
            Setting::Flag(b) => Some(*b),
            _ => None,
        })
    }

    pub fn list(&self, key: &str) -> Result<Option<&[String]>, ParseError> {
        self.get(key, "a list of strings", |v| match v {
            Setting::List(list) => Some(list.as_slice()),
            _ => None,
        })
    }

    fn get<'a, T>(
        &'a self,
        key: &str,
        shape: &str,
        read: impl FnOnce(&'a Setting) -> Option<T>,
    ) -> Result<Option<T>, ParseError> {
        self.values.get(key).map_or(Ok(None), |value| {
            read(value)
                .map(Some)
                .ok_or_else(|| ParseError(format!("ext.{}.{key} must be {shape}", self.name)))
        })
    }
}

/// The `[daemon]` table; every key is optional.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Daemon {
    pub idle_secs: Option<u64>,
    pub hibernate_secs: Option<u64>,
    pub drain_secs: Option<u64>,
    /// `loop` or `echo` (which needs no model, and offers the test
    /// fixtures).
    pub agent: Option<String>,
    pub crash_limit: Option<u64>,
}

/// The rules of a `policy.toml`, as `(tool, input, effect)`: each table of
/// its `rule` array names a `tool` (`*` for any), the `input` text a call
/// must hold if any, and its `effect`.
///
/// ```toml
/// [[rule]]
/// tool = "bash"
/// input = "rm -rf /"
/// effect = "deny"
/// ```
pub fn rules(text: &str) -> Result<Vec<Rule>, ParseError> {
    let doc: DocumentMut = text.parse()?;
    let Some(rules) = doc.get("rule") else {
        return Ok(Vec::new());
    };
    let rules = rules
        .as_array_of_tables()
        .ok_or_else(|| ParseError("rule must be an array of tables".into()))?;
    rules
        .iter()
        .map(|rule| {
            let need = |key| {
                string(rule, key, "rule.")?
                    .ok_or_else(|| ParseError(format!("a rule needs its {key}")))
            };
            Ok(Rule {
                tool: need("tool")?,
                input: string(rule, "input", "rule.")?,
                effect: need("effect")?.parse()?,
            })
        })
        .collect()
}

/// One of the operator's rules: what calls of `tool` (`*` for any) whose
/// input holds `input`, if any, meet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub tool: String,
    pub input: Option<String>,
    pub effect: Effect,
}

/// What a rule does to the calls it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

impl FromStr for Effect {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        match s {
            "allow" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "deny" => Ok(Self::Deny),
            other => Err(ParseError(format!(
                "rule.effect: no effect {other}: allow, ask or deny"
            ))),
        }
    }
}

impl Config {
    /// The file at `path`; a missing file is the empty config.
    pub fn read(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => text.parse().map_err(|e| invalid(path, &e)),
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
        let text = self.render(&old).map_err(|e| invalid(path, &e))?;
        write_private(path, text.as_bytes())
    }

    /// `old` with the schema's keys replaced by this config's: in place
    /// where they were, else at the end of their table.
    fn render(&self, old: &str) -> Result<String, ParseError> {
        let mut doc: DocumentMut = old.parse()?;
        let root = doc.as_table_mut();
        set(root, "model", self.model.as_deref());
        set(root, "agent", self.agent.as_deref());
        if !self.env.is_empty() || root.contains_key("env") {
            let env = root
                .entry("env")
                .or_insert_with(toml_edit::table)
                .as_table_like_mut()
                .ok_or_else(|| ParseError("env must be a table".into()))?;
            let gone: Vec<String> = env
                .iter()
                .filter(|(key, _)| !self.env.contains_key(*key))
                .map(|(key, _)| key.to_owned())
                .collect();
            for key in gone {
                env.remove(&key);
            }
            for (key, value) in &self.env {
                set(env, key, Some(value));
            }
        }
        Ok(doc.to_string())
    }
}

/// Sets `key` to the string `value`, or removes it for `None`. A value it
/// replaces keeps its comments.
fn set(table: &mut dyn TableLike, key: &str, value: Option<&str>) {
    let Some(value) = value else {
        table.remove(key);
        return;
    };
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            if old.as_str() != Some(value) {
                let decor = old.decor().clone();
                *old = value.into();
                *old.decor_mut() = decor;
            }
        }
        _ => {
            table.insert(key, toml_edit::value(value));
        }
    }
}

impl FromStr for Config {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        let doc: DocumentMut = text.parse()?;
        let mut config = Self {
            model: string(doc.as_table(), "model", "")?,
            agent: string(doc.as_table(), "agent", "")?,
            env: BTreeMap::new(),
            daemon: Daemon::default(),
            ext: Exts::default(),
        };
        if let Some(ext) = doc.get("ext") {
            config.ext = exts(ext)?;
        }
        if let Some(env) = doc.get("env") {
            let env = env
                .as_table_like()
                .ok_or_else(|| ParseError("env must be a table".into()))?;
            for (key, _) in env.iter() {
                if let Some(value) = string(env, key, "env.")? {
                    config.env.insert(key.to_owned(), value);
                }
            }
        }
        if let Some(daemon) = doc.get("daemon") {
            let t = daemon
                .as_table_like()
                .ok_or_else(|| ParseError("daemon must be a table".into()))?;
            let whole = |key| {
                t.get(key).map_or(Ok(None), |item| {
                    item.as_integer()
                        .and_then(|n| u64::try_from(n).ok())
                        .map(Some)
                        .ok_or_else(|| ParseError(format!("daemon.{key} must be a whole number")))
                })
            };
            config.daemon = Daemon {
                idle_secs: whole("idle_secs")?,
                hibernate_secs: whole("hibernate_secs")?,
                drain_secs: whole("drain_secs")?,
                agent: string(t, "agent", "daemon.")?,
                crash_limit: whole("crash_limit")?,
            };
        }
        Ok(config)
    }
}

/// The `[ext]` table.
fn exts(item: &Item) -> Result<Exts, ParseError> {
    let t = item
        .as_table_like()
        .ok_or_else(|| ParseError("ext must be a table".into()))?;
    let mut exts = Exts::default();
    for (key, item) in t.iter() {
        if key == "disable" {
            exts.disable = strings(item)
                .ok_or_else(|| ParseError("ext.disable must be a list of names".into()))?;
            continue;
        }
        let table = item
            .as_table_like()
            .ok_or_else(|| ParseError(format!("ext.{key} must be a table")))?;
        let mut values = BTreeMap::new();
        for (setting, item) in table.iter() {
            let value = if let Some(text) = item.as_str() {
                Setting::Text(text.to_owned())
            } else if let Some(n) = item.as_integer().and_then(|n| u64::try_from(n).ok()) {
                Setting::Whole(n)
            } else if let Some(b) = item.as_bool() {
                Setting::Flag(b)
            } else if let Some(list) = strings(item) {
                Setting::List(list)
            } else {
                let why = "must be a string, a whole number, true or false, or a list of strings";
                return Err(ParseError(format!("ext.{key}.{setting} {why}")));
            };
            values.insert(setting.to_owned(), value);
        }
        let name = key.to_owned();
        exts.tables.insert(name.clone(), Table { name, values });
    }
    Ok(exts)
}

/// The list of strings `item` is, if it is one.
fn strings(item: &Item) -> Option<Vec<String>> {
    let list = item.as_array()?;
    list.iter().map(|v| v.as_str().map(str::to_owned)).collect()
}

/// The string at `key`, if there is one; anything else there is an error.
fn string(table: &dyn TableLike, key: &str, scope: &str) -> Result<Option<String>, ParseError> {
    table.get(key).map_or(Ok(None), |item| {
        item.as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| ParseError(format!("{scope}{key} must be a string")))
    })
}

/// Why the file did not parse, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(String);

impl From<TomlError> for ParseError {
    fn from(e: TomlError) -> Self {
        Self(e.to_string().trim_end().to_owned())
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

fn invalid(path: &Path, e: &ParseError) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {e}", path.display()),
    )
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
    let mut file = crate::fs::private_file().open(&tmp)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| crate::fs::replace(&tmp, path));
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
            daemon: Daemon::default(),
            ext: Exts::default(),
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
count = 3

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
        assert_eq!(bad.to_string(), "model must be a string");
        let bad = "\n[env]\nK = 1\n".parse::<Config>().unwrap_err();
        assert_eq!(bad.to_string(), "env.K must be a string");
        let bad = "model = \"open\n".parse::<Config>().unwrap_err();
        assert!(bad.to_string().contains("line 1"), "{bad}");
        assert_eq!("".parse::<Config>().unwrap(), Config::default());
    }

    #[test]
    fn reads_the_daemon_table() {
        let text = "[daemon]\nidle_secs = 15\nagent = \"echo\"\n";
        let daemon = text.parse::<Config>().unwrap().daemon;
        assert_eq!(daemon.idle_secs, Some(15));
        assert_eq!(daemon.hibernate_secs, None);
        assert_eq!(daemon.agent.as_deref(), Some("echo"));
        for (bad, why) in [
            (
                "[daemon]\nidle_secs = -1\n",
                "daemon.idle_secs must be a whole number",
            ),
            ("daemon = 1\n", "daemon must be a table"),
        ] {
            assert_eq!(bad.parse::<Config>().unwrap_err().to_string(), why);
        }
        // The pane's writes leave the table as it was.
        let new = config(Some("m"), &[]);
        assert_eq!(new.render(text).unwrap(), format!("model = \"m\"\n{text}"));
    }

    #[test]
    fn reads_each_extensions_table() {
        let text = "[ext]\ndisable = [\"subagent\"]\n\n[ext.loop]\nmax_steps = 5\n\n[ext.policy]\nask = [\"bash\"]\nmode = \"auto\"\njudge = true\n";
        let ext = text.parse::<Config>().unwrap().ext;
        assert_eq!(ext.disable, ["subagent"]);
        assert_eq!(ext.table("loop").whole("max_steps"), Ok(Some(5)));
        let policy = ext.table("policy");
        assert_eq!(policy.list("ask"), Ok(Some(&["bash".to_owned()][..])));
        assert_eq!(policy.text("mode"), Ok(Some("auto")));
        assert_eq!(policy.flag("judge"), Ok(Some(true)));
        assert_eq!(policy.whole("missing"), Ok(None));
        assert_eq!(
            policy.whole("mode").unwrap_err().to_string(),
            "ext.policy.mode must be a whole number"
        );
        // A table no file names is empty.
        assert_eq!(ext.table("retry").whole("attempts"), Ok(None));
        for (bad, why) in [
            ("ext = 1\n", "ext must be a table"),
            (
                "[ext]\ndisable = \"x\"\n",
                "ext.disable must be a list of names",
            ),
            ("[ext]\nloop = 3\n", "ext.loop must be a table"),
        ] {
            assert_eq!(bad.parse::<Config>().unwrap_err().to_string(), why);
        }
    }

    #[test]
    fn reads_the_floors_rules() {
        let text = "[[rule]]\ntool = \"bash\"\ninput = \"rm -rf\"\neffect = \"deny\"\n\n[[rule]]\ntool = \"*\"\neffect = \"ask\"\n";
        let read = rules(text).unwrap();
        assert_eq!(
            read,
            [
                Rule {
                    tool: "bash".into(),
                    input: Some("rm -rf".into()),
                    effect: Effect::Deny
                },
                Rule {
                    tool: "*".into(),
                    input: None,
                    effect: Effect::Ask
                }
            ]
        );
        assert_eq!(rules("").unwrap(), []);
        let bad = rules("[[rule]]\ntool = \"x\"\n").unwrap_err();
        assert_eq!(bad.to_string(), "a rule needs its effect");
        let bad = rules("[[rule]]\ntool = \"x\"\neffect = \"maybe\"\n").unwrap_err();
        assert_eq!(
            bad.to_string(),
            "rule.effect: no effect maybe: allow, ask or deny"
        );
    }

    #[test]
    fn dotted_and_inline_env_keys_are_read() {
        let dotted: Config = "env.A = \"1\"\nenv.B = \"2\"\n".parse().unwrap();
        assert_eq!(dotted.env.len(), 2);
        assert_eq!(dotted.env["A"], "1");
        let inline: Config = "env = { B = \"2\" }\n".parse().unwrap();
        assert_eq!(inline.env["B"], "2");
    }

    #[test]
    fn writing_keeps_what_the_schema_does_not_know() {
        let old = "# mine\nmodel = \"a:b\" # which\nother = 3\n\n[env]\n# keys\nA = \"1\"\nB = \"2\"\n\n[x]\nk = 1\n";
        let new = config(Some("c:d"), &[("A", "one"), ("C", "3")]);
        let text = new.render(old).unwrap();
        assert_eq!(
            text,
            "# mine\nmodel = \"c:d\" # which\nother = 3\n\n[env]\n# keys\nA = \"one\"\nC = \"3\"\n\n[x]\nk = 1\n"
        );
        assert_eq!(text.parse::<Config>().unwrap(), new);
        // Idempotent.
        assert_eq!(new.render(&text).unwrap(), text);
    }

    #[test]
    fn writing_from_nothing_and_removing_keys() {
        let new = config(Some("openai:m"), &[("K", "a\"b\\c\nd")]);
        let text = new.render("").unwrap();
        assert_eq!(text.parse::<Config>().unwrap(), new);
        assert!(
            text.starts_with("model = \"openai:m\"\n\n[env]\nK = "),
            "{text}"
        );

        // A root key goes before the first table, never into it.
        let text = config(Some("m"), &[]).render("[env]\nK = \"v\"\n").unwrap();
        assert_eq!(text, "model = \"m\"\n[env]\n");
        assert_eq!(
            Config::default()
                .render("model = \"m\"\n[env]\nK = \"v\"\n")
                .unwrap(),
            "[env]\n"
        );
        assert!(Config::default().render("model = \"\n").is_err());
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
