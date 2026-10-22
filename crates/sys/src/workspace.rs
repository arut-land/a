//! Which workspace a directory belongs to, found the way git finds a
//! repository, so a session's workspace is the tree `git` itself would act
//! on from there.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

/// The git variables that change where a repository is found.
#[derive(Clone, Debug, Default)]
pub struct GitEnv {
    /// `GIT_DIR`: the repository is there, and without `GIT_WORK_TREE` the
    /// working tree is the current directory.
    pub dir: Option<PathBuf>,
    /// `GIT_WORK_TREE`: the top of the working tree.
    pub work_tree: Option<PathBuf>,
    /// `GIT_CEILING_DIRECTORIES`: directories the search never goes up into.
    pub ceilings: Vec<PathBuf>,
    /// `GIT_DISCOVERY_ACROSS_FILESYSTEM`: whether the search may leave the
    /// starting directory's filesystem.
    pub across_filesystems: bool,
}

impl GitEnv {
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// The variables as `var` gives them; empty ones count as unset.
    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        let path = |name| var(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        Self {
            dir: path("GIT_DIR"),
            work_tree: path("GIT_WORK_TREE"),
            ceilings: var("GIT_CEILING_DIRECTORIES").map_or_else(Vec::new, |v| {
                std::env::split_paths(&v)
                    .filter(|p| p.is_absolute())
                    .map(|p| canonical(&p))
                    .collect()
            }),
            across_filesystems: var("GIT_DISCOVERY_ACROSS_FILESYSTEM")
                .and_then(|v| v.into_string().ok())
                .is_some_and(|v| {
                    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
                }),
        }
    }
}

/// The workspace `dir` belongs to, with this process's git variables.
pub fn workspace_of(dir: &Path) -> PathBuf {
    workspace_in(dir, &GitEnv::from_env())
}

/// The workspace `dir` belongs to: the top of the git working tree it is
/// in, else `dir` itself, symlinks resolved either way. A `.git` file (a
/// linked worktree or a submodule) marks a working tree of its own.
pub fn workspace_in(dir: &Path, git: &GitEnv) -> PathBuf {
    search(dir, git, device)
}

/// [`workspace_in`], with `device` naming the filesystem a directory is on.
fn search(dir: &Path, git: &GitEnv, device: impl Fn(&Path) -> Option<u64>) -> PathBuf {
    let dir = canonical(dir);
    if let Some(top) = &git.work_tree {
        return canonical(&dir.join(top));
    }
    if git.dir.is_some() {
        return dir;
    }
    let start = device(&dir);
    for candidate in dir.ancestors() {
        // The starting directory is searched even when it is a ceiling; the
        // search only never climbs into one.
        if candidate != dir && git.ceilings.iter().any(|c| c == candidate) {
            break;
        }
        if !git.across_filesystems && start.is_some() && device(candidate) != start {
            break;
        }
        if is_repository_root(candidate) {
            return candidate.to_path_buf();
        }
    }
    dir
}

/// Whether `dir` holds a `.git` directory, or a `.git` file pointing at one.
fn is_repository_root(dir: &Path) -> bool {
    let git = dir.join(".git");
    match fs::metadata(&git) {
        Ok(meta) if meta.is_dir() => true,
        Ok(meta) if meta.is_file() => fs::read_to_string(&git).is_ok_and(|text| {
            text.strip_prefix("gitdir:")
                .is_some_and(|to| !to.trim().is_empty())
        }),
        _ => false,
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(unix)]
fn device(dir: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(dir).ok().map(|m| m.dev())
}

#[cfg(not(unix))]
fn device(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("a-ws-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(canonical(&root))
        }

        fn dir(&self, path: &str) -> PathBuf {
            let dir = self.0.join(path);
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn file(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn finds_the_working_tree_git_would() {
        let t = Tree::new("table");
        t.dir("plain/deep");
        t.dir("repo/.git");
        t.dir("repo/src/deep");
        t.file(
            "repo/linked/.git",
            "gitdir: /elsewhere/.git/worktrees/linked\n",
        );
        t.dir("repo/linked/src");
        t.file("repo/vendor/lib/.git", "gitdir: ../../.git/modules/lib\n");
        t.file("repo/junk/.git", "not a gitfile\n");
        t.dir("repo/junk/src");
        t.dir("repo/ceiling/inner");
        t.dir("other");

        let ceiling = GitEnv {
            ceilings: vec![t.0.join("repo/ceiling")],
            ..GitEnv::default()
        };
        let top_ceiling = GitEnv {
            ceilings: vec![t.0.join("repo")],
            ..GitEnv::default()
        };
        let work_tree = GitEnv {
            work_tree: Some(t.0.join("other")),
            dir: Some(t.0.join("repo/.git")),
            ..GitEnv::default()
        };
        let git_dir = GitEnv {
            dir: Some(t.0.join("repo/.git")),
            ..GitEnv::default()
        };
        let none = GitEnv::default();
        let cases: &[(&str, &GitEnv, &str)] = &[
            ("plain/deep", &none, "plain/deep"),
            ("repo", &none, "repo"),
            ("repo/src/deep", &none, "repo"),
            ("repo/linked/src", &none, "repo/linked"),
            ("repo/vendor/lib", &none, "repo/vendor/lib"),
            ("repo/junk/src", &none, "repo"),
            ("repo/ceiling/inner", &ceiling, "repo/ceiling/inner"),
            ("repo/ceiling", &ceiling, "repo"),
            ("repo/src/deep", &top_ceiling, "repo/src/deep"),
            ("repo", &top_ceiling, "repo"),
            ("repo/src", &work_tree, "other"),
            ("repo/src", &git_dir, "repo/src"),
        ];
        for &(from, git, want) in cases {
            assert_eq!(
                workspace_in(&t.0.join(from), git),
                t.0.join(want),
                "from {from} with {git:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_resolve_to_the_real_tree() {
        let t = Tree::new("links");
        t.dir("repo/.git");
        t.dir("repo/src");
        std::os::unix::fs::symlink(t.0.join("repo/src"), t.0.join("alias")).unwrap();
        assert_eq!(
            workspace_in(&t.0.join("alias"), &GitEnv::default()),
            t.0.join("repo")
        );
    }

    #[test]
    fn the_search_stays_on_one_filesystem() {
        let t = Tree::new("mounts");
        t.dir("repo/.git");
        let mount = t.dir("repo/mnt");
        t.dir("repo/mnt/deep");
        // Everything under `repo/mnt` is another filesystem.
        let device = |dir: &Path| Some(u64::from(dir.starts_with(&mount)));
        let across = GitEnv {
            across_filesystems: true,
            ..GitEnv::default()
        };
        let from = t.0.join("repo/mnt/deep");
        assert_eq!(search(&from, &GitEnv::default(), device), from);
        assert_eq!(search(&from, &across, device), t.0.join("repo"));
        assert_eq!(
            search(&t.0.join("repo"), &GitEnv::default(), device),
            t.0.join("repo")
        );
    }

    #[test]
    fn reads_the_variables() {
        let current = std::env::current_dir().unwrap();
        let root = current.ancestors().last().unwrap();
        let ceilings = std::env::join_paths([Path::new("relative"), root]).unwrap();
        let git = GitEnv::from_vars(|name| match name {
            "GIT_DIR" => Some("".into()),
            "GIT_WORK_TREE" => Some("/w".into()),
            "GIT_CEILING_DIRECTORIES" => Some(ceilings.clone()),
            "GIT_DISCOVERY_ACROSS_FILESYSTEM" => Some("True".into()),
            _ => None,
        });
        assert_eq!(git.dir, None);
        assert_eq!(git.work_tree, Some(PathBuf::from("/w")));
        assert_eq!(git.ceilings, vec![canonical(root)]);
        assert!(git.across_filesystems);
    }
}
