//! The walk: every file of a root that git (or, outside git, the usual
//! tool and cache directories) does not ignore, as ripgrep walks it.

use std::collections::HashMap;
use std::fs::Metadata;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;
use ignore::overrides::Override;

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
    pub(crate) fn walk(&self, stop: &dyn Fn() -> bool) -> Vec<Found> {
        self.files_under(&self.root, None, stop)
    }

    /// Every file under `dir` (relative to the root): what a directory
    /// that appeared brings.
    pub(crate) fn walk_dir(&self, dir: &str) -> Vec<Found> {
        self.files_under(&self.root.join(dir), None, &|| false)
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
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let Some(path) = self.relative(entry.path()) else {
                continue;
            };
            if is_git_path(&path) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            found.push(Found {
                path,
                size: meta.len(),
                modified: modified(&meta),
            });
        }
        found
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
