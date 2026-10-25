//! What a walk of a root outside git leaves out: the trees of tools'
//! output and package caches, which no `.gitignore` names there.

use std::path::Path;

/// Directories excluded when walking a non-git root.
pub(crate) const IGNORED_DIRS: &[&str] = &[
    // various dev tools that can be meet in the developer app
    "node_modules",
    "__pycache__",
    "venv",
    ".venv",
    "target/debug",
    "target/release",
    "target/rust-analyzer",
    "target/criterion",
    // Language package caches in non-git roots.
    "go/pkg/mod",
    ".cargo/registry",
    ".rustup/toolchains",
    ".gradle/caches",
    ".m2/repository",
    ".npm/_cacache",
    ".pub-cache",
];

pub(crate) fn non_git_repo_overrides(base_path: &Path) -> Option<ignore::overrides::Override> {
    let mut builder = ignore::overrides::OverrideBuilder::new(base_path);
    for dir in IGNORED_DIRS {
        builder.add(&format!("!**/{dir}/")).ok()?;
    }
    builder.build().ok()
}
