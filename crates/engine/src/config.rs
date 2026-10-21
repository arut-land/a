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
use std::time::{Duration, Instant, SystemTime};

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

/// How long a hand edit may go unseen: the file is looked at (one stat)
/// at most this often, not once per call.
pub const FLOOR: Duration = Duration::from_secs(2);

/// A file read again only once it changes, so hand edits apply without a
/// daemon restart. Clients change it through the daemon ([`Watched::change`]),
/// which is then its one writer and sees its own writes at once.
pub struct Watched {
    path: PathBuf,
    read: Mutex<Option<Read>>,
    /// How long after a look the file is trusted not to have changed.
    floor: Duration,
}

/// The file as last read: its stamp then, and when it was last looked at.
struct Read {
    stamp: Stamp,
    file: Loaded,
    looked: Instant,
}

impl Watched {
    pub fn new(path: PathBuf, floor: Duration) -> Self {
        Self {
            path,
            read: Mutex::new(None),
            floor,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file as it is now, or why it could not be read.
    pub fn get(&self) -> Loaded {
        let mut read = lock(&self.read);
        if let Some(last) = &*read
            && last.looked.elapsed() < self.floor
        {
            return Arc::clone(&last.file);
        }
        let now = stamp(&self.path);
        let looked = Instant::now();
        match read.as_mut() {
            Some(last) if last.stamp == now => {
                last.looked = looked;
                Arc::clone(&last.file)
            }
            _ => {
                let file = Arc::new(load(&self.path));
                *read = Some(Read {
                    stamp: now,
                    file: Arc::clone(&file),
                    looked,
                });
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
        *read = Some(Read {
            stamp: stamp(&self.path),
            file: Arc::clone(&file),
            looked: Instant::now(),
        });
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

/// The layers, and the overrides on top of them.
pub struct Settings {
    /// `$A_HOME/config.toml`.
    pub home: Watched,
    /// `[daemon] agent`: wins over both files.
    pub agent: Option<String>,
    /// Each workspace's `.a/config.toml`, shared by all its sessions.
    workspaces: Mutex<HashMap<PathBuf, Arc<Watched>>>,
    floor: Duration,
}

impl Settings {
    pub fn new(home: PathBuf, agent: Option<String>) -> Self {
        Self::looking(home, agent, FLOOR)
    }

    /// The same, looking at the files at most once per `floor`.
    pub fn looking(home: PathBuf, agent: Option<String>, floor: Duration) -> Self {
        Self {
            home: Watched::new(home, floor),
            agent,
            workspaces: Mutex::default(),
            floor,
        }
    }

    /// The workspace's file: parsed once per change for every session in
    /// the workspace, as the home file is.
    fn workspace(&self, workspace: &Path) -> Loaded {
        let watched = Arc::clone(
            lock(&self.workspaces)
                .entry(workspace.to_path_buf())
                .or_insert_with(|| {
                    let path = workspace.join(".a").join("config.toml");
                    Arc::new(Watched::new(path, self.floor))
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
    /// which layer named it: the workspace's file over the home file over
    /// the default. A session's own choice is an extension's state, over
    /// these ([`ext::ModelSpec`]).
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

    pub fn fast_model(&self, workspace: &Path) -> Result<Option<String>, String> {
        let local = self.workspace(workspace);
        if let Some(model) = &local.as_ref().as_ref().map_err(Clone::clone)?.fast_model {
            return Ok(Some(model.clone()));
        }
        Ok(self
            .home
            .get()
            .as_ref()
            .as_ref()
            .map_err(Clone::clone)?
            .fast_model
            .clone())
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
