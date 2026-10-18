//! Staging repoints symlinks, which Windows grants only in developer mode.
//! Until the daemon runs there, a build runs where it was installed.

use std::io;
use std::path::{Path, PathBuf};

/// The version every build id starts with.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The staging area of one home.
#[derive(Clone, Debug)]
pub struct Runtime {
    dir: PathBuf,
}

/// A build, under the id clients and daemons compare in their hello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    pub id: String,
    pub exe: PathBuf,
}

impl Runtime {
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where a staged daemon would run from.
    pub fn current_exe(&self) -> PathBuf {
        self.dir.join("current").join("a")
    }

    /// Nothing is staged, so no build outranks a running one.
    pub fn current(&self) -> Option<String> {
        None
    }

    /// `exe` itself, unstaged.
    pub fn stage(&self, exe: &Path) -> io::Result<Staged> {
        Ok(Staged {
            id: format!("{VERSION}+unstaged"),
            exe: exe.to_owned(),
        })
    }
}
