//! The settings a session runs with, layered: built-in defaults <
//! `$A_HOME/config.toml` < the workspace's `.a/config.toml` < the session's
//! own. Provider variables (keys, base URLs) come from the session's client
//! first, then the home file's `[env]`; never from the daemon's own
//! environment, which is whichever client's started it, and never from a
//! workspace, which is anyone's checkout.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use protocol::{Environment, HomeSettings, SettingsChange};

use crate::lock;

/// The model a session calls when no layer names one.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

/// A `config.toml`, read and edited by [`sys::config`].
pub use sys::config::Config as File;

/// The file at `path`; a missing one is empty.
fn load(path: &Path) -> Result<File, String> {
    File::read(path).map_err(|e| e.to_string())
}

/// When a file last changed, as far as telling a rewrite apart goes.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// A file as last read, or why it could not be.
pub type Loaded = Arc<Result<File, String>>;

/// A file read again only once it changes, so hand edits apply without a
/// daemon restart. Clients change it through the daemon ([`Watched::change`]),
/// which is then its one writer.
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
                let file = Arc::new(load(&self.path));
                *read = Some((now, Arc::clone(&file)));
                file
            }
        }
    }

    /// Makes `change` to the file as it is on disk, and keeps what it
    /// wrote as read: changes are made one at a time, so none undoes
    /// another, and the next read sees this one even when the rewrite left
    /// the file's size and time as they were.
    pub fn change(&self, change: &SettingsChange) -> Result<Loaded, String> {
        let mut read = lock(&self.read);
        let mut file = load(&self.path)?;
        let given = |value: &str| Some(value.trim().to_owned()).filter(|v| !v.is_empty());
        if let Some(model) = &change.model {
            file.model = given(model);
        }
        if let Some(agent) = &change.agent {
            file.agent = given(agent);
        }
        for (name, value) in &change.env {
            match value {
                Some(value) => file.env.insert(name.clone(), value.clone()),
                None => file.env.remove(name),
            };
        }
        file.write(&self.path).map_err(|e| e.to_string())?;
        let file = Arc::new(Ok(file));
        *read = Some((stamp(&self.path), Arc::clone(&file)));
        Ok(file)
    }
}

/// What a settings pane shows of `file`.
pub fn shown(file: &File) -> HomeSettings {
    HomeSettings {
        model: file.model.clone(),
        agent: file.agent.clone(),
        env: file.env.clone(),
    }
}

/// The layer a setting came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default,
    Home,
    Workspace,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("the built-in default"),
            Self::Home => f.write_str("the home config.toml"),
            Self::Workspace => f.write_str("the workspace's .a/config.toml"),
        }
    }
}

/// The layers: a session's own choice, if an extension keeps one, sits
/// on top of them ([`ext::ModelSpec`]).
pub struct Settings {
    /// `$A_HOME/config.toml`.
    pub home: Watched,
    /// `[daemon] agent`: wins over both files.
    pub agent: Option<String>,
    /// Each workspace's `.a/config.toml`, shared by all its sessions.
    workspaces: Mutex<HashMap<PathBuf, Arc<Watched>>>,
}

impl Settings {
    pub fn new(home: PathBuf, agent: Option<String>) -> Self {
        Self {
            home: Watched::new(home),
            agent,
            workspaces: Mutex::default(),
        }
    }

    /// The workspace's file: parsed once per change for every session in
    /// the workspace, as the home file is.
    fn workspace(&self, workspace: &Path) -> Loaded {
        let watched = Arc::clone(
            lock(&self.workspaces)
                .entry(workspace.to_path_buf())
                .or_insert_with(|| {
                    Arc::new(Watched::new(workspace.join(".a").join("config.toml")))
                }),
        );
        watched.get()
    }

    /// The name of the agent a session in `workspace` runs, if configured:
    /// otherwise the first an extension registers. A file that cannot be
    /// read is passed over here; the model call reports it.
    pub fn agent(&self, workspace: &Path) -> Option<String> {
        if let Some(name) = &self.agent {
            return Some(name.clone());
        }
        let named = |file: &Result<File, String>| file.as_ref().ok()?.agent.clone();
        named(&self.workspace(workspace)).or_else(|| named(&self.home.get()))
    }

    /// The model spec the layers name for a session in `workspace`, and
    /// the layer that names it: the workspace's file over the home file
    /// over the default.
    pub fn model(&self, workspace: &Path) -> Result<(String, Source), String> {
        let local = self.workspace(workspace);
        if let Some(model) = &local.as_ref().as_ref().map_err(Clone::clone)?.model {
            return Ok((model.clone(), Source::Workspace));
        }
        let home = self.home.get();
        let home = home.as_ref().as_ref().map_err(Clone::clone)?;
        Ok(home.model.clone().map_or_else(
            || (DEFAULT_MODEL.to_owned(), Source::Default),
            |model| (model, Source::Home),
        ))
    }

    /// Variable `name` for a session whose client environment is `client`:
    /// the client's, else the home file's `[env]`.
    pub fn var(&self, client: Option<&Environment>, name: &str) -> Option<String> {
        let set = |value: &String| !value.trim().is_empty();
        if let Some(value) = client.and_then(|env| env.vars.get(name)).filter(|v| set(v)) {
            return Some(value.clone());
        }
        let home = self.home.get();
        let home = home.as_ref().as_ref().ok()?;
        home.env.get(name).filter(|v| set(v)).cloned()
    }
}

#[cfg(test)]
mod tests;

/// The layers as every extension asks them.
impl ext::Layers for Settings {
    fn model(&self, workspace: &Path) -> Result<(String, String), String> {
        Self::model(self, workspace).map(|(model, from)| (model, from.to_string()))
    }
}
