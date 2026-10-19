//! The shell's durable state.

use std::path::{Path, PathBuf};

use ext::Fold;
use serde::{Deserialize, Serialize};

/// Where the session's last command left off, and the client directory it
/// started from, as the shell records it.
#[derive(Debug, Default)]
pub struct Cwd {
    dir: Option<PathBuf>,
    base: Option<PathBuf>,
}

/// A command run from `base` ended in `cwd`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Moved {
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    base: Option<PathBuf>,
}

impl Cwd {
    /// The change recording that a command run from `base` ended in `dir`.
    pub fn change(dir: &Path, base: Option<&Path>) -> Moved {
        Moved {
            cwd: Some(dir.to_path_buf()),
            base: base.map(Path::to_path_buf),
        }
    }

    /// The last command's directory, unless the client has since moved
    /// away from where it ran.
    pub fn after(&self, base: Option<&Path>) -> Option<&Path> {
        self.dir.as_deref().filter(|_| self.base.as_deref() == base)
    }
}

impl Fold for Cwd {
    const NAME: &'static str = "cwd";
    type Change = Moved;

    fn apply(&mut self, moved: &Moved) {
        self.dir.clone_from(&moved.cwd);
        self.base.clone_from(&moved.base);
    }
}
