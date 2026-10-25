//! The walk: every file of a root that git (or, outside git, the usual
//! tool and cache directories) does not ignore, as ripgrep walks it.

use std::collections::HashMap;
use std::fs::Metadata;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::UNIX_EPOCH;

use ignore::overrides::Override;
use ignore::{DirEntry, WalkBuilder, WalkState};

use super::ignore::non_git_repo_overrides;
use super::types::Found;

/// How a root is walked: gitignore rules inside git; outside it, hidden
/// files and the usual tool and cache directories are left out too.
pub(crate) struct Rules {
    root: PathBuf,
    git: bool,
    overrides: Option<Override>,
}

impl Rules {
    pub(crate) fn of(root: &Path) -> Self {
        let git = root.ancestors().any(|dir| dir.join(".git").exists());
        Self {
            root: root.to_owned(),
            git,
            overrides: (!git).then(|| non_git_repo_overrides(root)).flatten(),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// A walk of `dir`, which is the root or under it: ignore files of
    /// the directories above `dir` apply to it as they do in a walk from
    /// the root.
    fn builder(&self, dir: &Path) -> WalkBuilder {
        let mut walk = WalkBuilder::new(dir);
        walk.hidden(!self.git)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .ignore(true)
            .parents(true)
            .follow_links(false);
        if let Some(overrides) = &self.overrides {
            walk.overrides(overrides.clone());
        }
        walk
    }

    /// Every file under the root, unless `stop` says to give up first.
    /// The walk's threads are its own, scoped to it: ripgrep's parallel
    /// walker, a third of the time a single thread takes on a large tree
    /// (docs/perf/index.md).
    pub(crate) fn walk(&self, stop: &(dyn Fn() -> bool + Sync)) -> Vec<Found> {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
        let found = Mutex::new(Vec::new());
        self.builder(&self.root)
            .threads(threads)
            .build_parallel()
            .run(|| {
                // Each thread's files, handed over as its walk ends.
                let mut batch = Batch(Vec::new(), &found);
                Box::new(move |entry| {
                    if batch.0.len() % 256 == 0 && stop() {
                        return WalkState::Quit;
                    }
                    if let Some(file) = entry.ok().and_then(|entry| self.found(&entry)) {
                        batch.0.push(file);
                    }
                    WalkState::Continue
                })
            });
        found.into_inner().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every file under `dir` (relative to the root): what a directory
    /// that appeared brings.
    pub(crate) fn walk_dir(&self, dir: &str) -> Vec<Found> {
        self.walk_dir_until(dir, &|| false)
    }

    /// [`Rules::walk_dir`], unless `stop` says to give up first.
    pub(crate) fn walk_dir_until(&self, dir: &str, stop: &dyn Fn() -> bool) -> Vec<Found> {
        self.files_under(&self.root.join(dir), None, stop)
    }

    /// The files among `names` (relative paths, all in directory `dir`)
    /// that a walk would find, and what it would find of each: a path
    /// not returned is ignored, or gone, or not a file.
    pub(crate) fn files_in(&self, dir: &str, names: &[&str]) -> Vec<Found> {
        let mut found = self.files_under(&self.root.join(dir), Some(1), &|| false);
        found.retain(|f| names.contains(&f.path.as_str()));
        found
    }

    /// Whether a walk from the root enters `dir` (relative): neither it
    /// nor a directory above it is ignored. `memo` keeps what listings
    /// already showed, for the next directory asked about.
    pub(crate) fn enters(&self, dir: &str, memo: &mut HashMap<String, bool>) -> bool {
        if dir.is_empty() {
            return true;
        }
        if let Some(&known) = memo.get(dir) {
            return known;
        }
        let parent = dir.rfind('/').map_or("", |slash| &dir[..slash]);
        if self.enters(parent, memo) {
            let listed = self
                .builder(&self.root.join(parent))
                .max_depth(Some(1))
                .build();
            for entry in listed.flatten() {
                if entry.depth() == 1
                    && entry.file_type().is_some_and(|t| t.is_dir())
                    && let Some(child) = self.relative(entry.path())
                {
                    memo.insert(child, true);
                }
            }
        }
        *memo.entry(dir.to_owned()).or_insert(false)
    }

    fn files_under(&self, dir: &Path, depth: Option<usize>, stop: &dyn Fn() -> bool) -> Vec<Found> {
        let mut found = Vec::new();
        for entry in self.builder(dir).max_depth(depth).build() {
            if found.len() % 256 == 0 && stop() {
                break;
            }
            found.extend(entry.ok().and_then(|entry| self.found(&entry)));
        }
        found
    }

    /// The file a walk's `entry` is, if it is one the index holds.
    fn found(&self, entry: &DirEntry) -> Option<Found> {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            return None;
        }
        let path = self.relative(entry.path())?;
        if is_git_path(&path) {
            return None;
        }
        let meta = entry.metadata().ok()?;
        Some(Found {
            path,
            size: meta.len(),
            modified: modified(&meta),
        })
    }

    /// `path` as the index names it: relative to the root, `/`-separated.
    /// Paths that are not UTF-8 are left out: a query could not name them.
    pub(crate) fn relative(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.root).ok()?.to_str()?;
        Some(if MAIN_SEPARATOR == '/' {
            relative.to_owned()
        } else {
            relative.replace(MAIN_SEPARATOR, "/")
        })
    }
}

struct Batch<'a>(Vec<Found>, &'a Mutex<Vec<Found>>);

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        (self.1.lock().unwrap_or_else(PoisonError::into_inner)).append(&mut self.0);
    }
}

/// The repository's own files, which no walk shows.
pub(crate) fn is_git_path(relative: &str) -> bool {
    relative == ".git" || relative.starts_with(".git/") || relative.contains("/.git/")
}

fn modified(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}
