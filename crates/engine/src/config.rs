//! The settings a session runs with, layered: built-in defaults <
//! `$A_HOME/config.toml` < the workspace's `.a/config.toml` < the session's
//! own. Provider variables (keys, base URLs) come from the session's client
//! first, then the home file's `[env]`; never from the daemon's own
//! environment, which is whichever client's started it, and never from a
//! workspace, which is anyone's checkout.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use protocol::{Environment, Overrides};

use crate::lock;

/// The model a session calls when no layer names one.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";

/// A `config.toml`, read by [`sys::config`], which the app's settings pane
/// also writes.
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
                let file = Arc::new(load(&self.path));
                *read = Some((now, Arc::clone(&file)));
                file
            }
        }
    }
}

/// The layer a setting came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default,
    Home,
    Workspace,
    /// The session's own, set by its entry `seq`.
    Session(u64),
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("the built-in default"),
            Self::Home => f.write_str("the home config.toml"),
            Self::Workspace => f.write_str("the workspace's .a/config.toml"),
            Self::Session(seq) => write!(f, "this session, at entry {seq}"),
        }
    }
}

/// The layers, and the overrides on top of them.
pub struct Settings {
    /// `$A_HOME/config.toml`.
    pub home: Watched,
    /// `[daemon] agent`: wins over both files.
    pub agent: Option<agent::Kind>,
}

impl Settings {
    pub fn new(home: PathBuf, agent: Option<agent::Kind>) -> Self {
        Self {
            home: Watched::new(home),
            agent,
        }
    }

    /// The workspace's file, read afresh: it is only consulted when a
    /// session starts or calls the model.
    fn workspace(workspace: &Path) -> Result<File, String> {
        load(&workspace.join(".a").join("config.toml"))
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

    /// The model spec for a session in `workspace` that sets none itself.
    pub fn model(&self, workspace: &Path) -> Result<String, String> {
        self.model_in(workspace, (Overrides::default(), None))
            .map(|(model, _)| model)
    }

    /// The model spec for a session in `workspace` with its own settings
    /// `session` (and the seq of the entry that set them), and the layer
    /// that named it: the session's own over every other.
    pub fn model_in(
        &self,
        workspace: &Path,
        session: (Overrides, Option<u64>),
    ) -> Result<(String, Source), String> {
        if let (Overrides { model: Some(model) }, seq) = session {
            return Ok((model, Source::Session(seq.unwrap_or_default())));
        }
        let local = Self::workspace(workspace)?;
        if let Some(model) = local.model {
            return Ok((model, Source::Workspace));
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
