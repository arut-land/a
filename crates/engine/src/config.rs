//! The settings a session runs with, layered: built-in defaults <
//! `$A_HOME/config.toml` < the workspace's `.a/config.toml` < `A_MODEL` and
//! `A_AGENT`. Provider variables (keys, base URLs) come from the session's
//! client first, then the home file's `[env]`, then the daemon's own
//! environment; a workspace, being anyone's checkout, never sets them.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use protocol::Environment;
use serde::Deserialize;

use crate::lock;

/// The model a session calls when no layer names one.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

/// A `config.toml`. Unknown keys are ignored, so a newer file still loads.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
pub struct File {
    /// `host[@protocol]:model`, or a bare model on `OpenAI`.
    pub model: Option<String>,
    /// `chat` or `echo`.
    pub agent: Option<String>,
    /// Provider variables, for sessions whose client has none.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl File {
    /// The file at `path`; a missing one is empty.
    fn read(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

/// When a file last changed, as far as telling a rewrite apart goes.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// A file as last read, or why it could not be.
pub type Loaded = Arc<Result<File, String>>;

/// A file read again only once it changes, so edits apply without a
/// daemon restart.
pub struct Watched {
    path: PathBuf,
    read: Mutex<Option<(Stamp, Loaded)>>,
}

impl Watched {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            read: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file as it is now, or why it could not be read.
    pub fn get(&self) -> Loaded {
        let now = stamp(&self.path);
        let mut read = lock(&self.read);
        match &*read {
            Some((then, file)) if *then == now => Arc::clone(file),
            _ => {
                let file = Arc::new(File::read(&self.path));
                *read = Some((now, Arc::clone(&file)));
                file
            }
        }
    }
}

/// The layers, and the overrides on top of them.
pub struct Settings {
    /// `$A_HOME/config.toml`.
    pub home: Watched,
    /// `A_MODEL`: wins over both files.
    pub model: Option<String>,
    /// `A_AGENT`: wins over both files.
    pub agent: Option<agent::Kind>,
    /// The daemon's own environment, the last place a variable is looked
    /// up.
    pub process: fn(&str) -> Option<String>,
}

impl Settings {
    /// Settings from `home` and this process's environment.
    pub fn new(home: PathBuf, model: Option<String>, agent: Option<agent::Kind>) -> Self {
        Self {
            home: Watched::new(home),
            model,
            agent,
            process: llm::protocol::env,
        }
    }

    /// The workspace's file, read afresh: it is only consulted when a
    /// session starts or calls the model.
    fn workspace(workspace: &Path) -> Result<File, String> {
        File::read(&workspace.join(".a").join("config.toml"))
    }

    /// The agent a session in `workspace` runs. A file that cannot be read
    /// is passed over here; the model call reports it.
    pub fn agent(&self, workspace: &Path) -> agent::Kind {
        if let Some(kind) = self.agent {
            return kind;
        }
        let named = |file: &Result<File, String>| file.as_ref().ok()?.agent.clone();
        let name = named(&Self::workspace(workspace)).or_else(|| named(&self.home.get()));
        name.and_then(|name| {
            name.parse()
                .map_err(|e| eprintln!("agent `{name}` in config.toml: {e}"))
                .ok()
        })
        .unwrap_or_default()
    }

    /// The model spec for a session in `workspace`.
    pub fn model(&self, workspace: &Path) -> Result<String, String> {
        if let Some(model) = &self.model {
            return Ok(model.clone());
        }
        let local = Self::workspace(workspace)?;
        let home = self.home.get();
        let home = home.as_ref().as_ref().map_err(Clone::clone)?;
        Ok(local
            .model
            .or_else(|| home.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned()))
    }

    /// Variable `name` for a session whose client environment is `client`:
    /// the client's, else the home file's `[env]`, else the daemon's.
    pub fn var(&self, client: Option<&Environment>, name: &str) -> Option<String> {
        let set = |value: &String| !value.trim().is_empty();
        if let Some(value) = client.and_then(|env| env.vars.get(name)).filter(|v| set(v)) {
            return Some(value.clone());
        }
        if let Ok(home) = &*self.home.get()
            && let Some(value) = home.env.get(name).filter(|v| set(v))
        {
            return Some(value.clone());
        }
        (self.process)(name)
    }
}

#[cfg(test)]
mod tests;
