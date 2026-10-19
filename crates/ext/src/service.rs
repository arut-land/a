//! Services: typed capabilities one extension provides and others build
//! on, by type rather than by knowing each other. Each has at most one
//! provider per registry, and an extension that needs one no extension
//! provides is refused as it installs.

use std::path::{Path, PathBuf};
use std::process::Command;

use protocol::Environment;

use crate::{Cx, SpawnSpec, States};

/// Where a session's commands run. The shell provides it; what works
/// where they do (`edit`, the context the model is first given) asks it.
pub trait CwdOf: Send + Sync {
    /// The directory, for a session in `workspace` whose client's
    /// environment is `env`, with its extensions' `states`.
    fn dir(&self, workspace: &Path, env: Option<&Environment>, states: &States) -> PathBuf;
}

/// Starts the processes tools run: one extension owns how (the shell).
pub trait Spawner: Send + Sync {
    /// The process `spawn` describes, as the `spawn` point leaves it,
    /// ready to start.
    fn process(&self, cx: &Cx, spawn: SpawnSpec) -> Result<Command, String>;
}
