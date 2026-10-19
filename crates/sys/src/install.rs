//! "Install CLI": an `a` on the user's `PATH` that is a link to
//! `<home>/runtime/current/a`, so the command line always runs the build the
//! daemon runs. It is made only where no `a` is found already; a link an
//! earlier install made (to another home's or a pinned build) may be
//! replaced, anything else is left alone.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::Runtime;

/// What stands where the CLI would be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliLink {
    /// `path` links to this home's `runtime/current/a`.
    Installed { path: PathBuf },
    /// `path` links to a staged `a`, but not this home's `current`: an
    /// earlier install made it, and it may be replaced.
    Stale { path: PathBuf, target: PathBuf },
    /// `path` is an `a` installed some other way; it is never touched.
    Foreign { path: PathBuf },
    /// No `a` in the directory or on `PATH`: one can go at `path`.
    /// `on_path` says whether its directory is on `PATH`.
    Absent { path: PathBuf, on_path: bool },
}

impl CliLink {
    /// What `a` resolves to with `dir` (where it would be installed) in
    /// front of `search` (a `PATH` value).
    pub fn find(runtime: &Runtime, dir: &Path, search: &OsStr) -> Self {
        let candidate = dir.join("a");
        if fs::symlink_metadata(&candidate).is_ok() {
            return classify(runtime, candidate);
        }
        let on_path = std::env::split_paths(search).any(|d| same_dir(&d, dir));
        std::env::split_paths(search)
            .map(|d| d.join("a"))
            .find(|a| is_executable(a))
            .map_or(
                Self::Absent {
                    path: candidate,
                    on_path,
                },
                |found| classify(runtime, found),
            )
    }

    /// Links `dir/a` to `runtime/current/a`, or repoints a stale link when
    /// `replace` allows it, and returns what is there now.
    pub fn install(
        runtime: &Runtime,
        dir: &Path,
        search: &OsStr,
        replace: bool,
    ) -> io::Result<Self> {
        if runtime.current().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no build of a is staged yet",
            ));
        }
        let path = match Self::find(runtime, dir, search) {
            installed @ Self::Installed { .. } => return Ok(installed),
            Self::Foreign { path } => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} is an a this app did not install; remove it to install here",
                        path.display()
                    ),
                ));
            }
            Self::Stale { path, .. } if !replace => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} links to another build of a", path.display()),
                ));
            }
            Self::Stale { path, .. } | Self::Absent { path, .. } => path,
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_file_name(format!(".a.{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        symlink(&runtime.current_exe(), &tmp)?;
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        Ok(classify(runtime, path))
    }
}

fn classify(runtime: &Runtime, path: PathBuf) -> CliLink {
    let Ok(target) = fs::read_link(&path) else {
        return CliLink::Foreign { path };
    };
    let target = path.parent().unwrap_or(Path::new("/")).join(target);
    let staged = |t: &Path| -> Option<PathBuf> {
        let build = t.parent()?;
        let dir = build.parent()?;
        let id = build.file_name()?.to_str()?;
        (t.file_name()? == "a" && (id == "current" || id.contains('+'))).then(|| dir.to_owned())
    };
    match staged(&target) {
        Some(dir)
            if target.parent().is_some_and(|b| b.ends_with("current"))
                && same_dir(&dir, runtime.dir()) =>
        {
            CliLink::Installed { path }
        }
        Some(dir) if dir.file_name().is_some_and(|n| n == "runtime") => {
            CliLink::Stale { path, target }
        }
        _ => CliLink::Foreign { path },
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || fs::canonicalize(a).is_ok_and(|a| fs::canonicalize(b).is_ok_and(|b| a == b))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Granted only in developer mode, else refused: the app's installer is
/// macOS's for now.
#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        root: PathBuf,
        runtime: Runtime,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("a-install-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(root.join("src/a"), b"build").unwrap();
            let runtime = Runtime::new(root.join("home/runtime"));
            runtime.stage(&root.join("src/a")).unwrap();
            Self { root, runtime }
        }

        fn dir(&self, name: &str) -> PathBuf {
            let dir = self.root.join(name);
            fs::create_dir_all(&dir).unwrap();
            dir
        }
    }

    fn joined(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn installs_where_nothing_is_and_then_reports_it() {
        let f = Fixture::new("fresh");
        let (bin, other) = (f.root.join("local/bin"), f.dir("other"));
        let search = joined(&[&other, &bin]);
        let path = bin.join("a");
        assert_eq!(
            CliLink::find(&f.runtime, &bin, &search),
            CliLink::Absent {
                path: path.clone(),
                on_path: true
            }
        );
        let installed = CliLink::install(&f.runtime, &bin, &search, false).unwrap();
        assert_eq!(installed, CliLink::Installed { path: path.clone() });
        assert_eq!(fs::read(&path).unwrap(), b"build");
        // Again: nothing to do.
        assert_eq!(
            CliLink::install(&f.runtime, &bin, &search, false).unwrap(),
            installed
        );
        // Found on PATH from any directory asked about.
        assert_eq!(CliLink::find(&f.runtime, &other, &search), installed);
        assert!(matches!(
            CliLink::find(&f.runtime, &f.dir("off"), &joined(&[])),
            CliLink::Absent { on_path: false, .. }
        ));
    }

    #[test]
    fn leaves_a_foreign_a_alone_and_replaces_a_stale_link_only_when_asked() {
        let f = Fixture::new("taken");
        let (bin, brew) = (f.dir("bin"), f.dir("brew"));
        fs::write(brew.join("a"), b"brew").unwrap();
        fs::set_permissions(brew.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        let search = joined(&[&brew, &bin]);
        assert_eq!(
            CliLink::find(&f.runtime, &bin, &search),
            CliLink::Foreign {
                path: brew.join("a")
            }
        );
        assert!(CliLink::install(&f.runtime, &bin, &search, true).is_err());
        assert_eq!(fs::read(brew.join("a")).unwrap(), b"brew");

        // A link into another home's runtime.
        let other = f.root.join("old-home/runtime/current/a");
        symlink(&other, bin.join("a")).unwrap();
        let search = joined(&[&bin]);
        assert_eq!(
            CliLink::find(&f.runtime, &bin, &search),
            CliLink::Stale {
                path: bin.join("a"),
                target: other
            }
        );
        assert!(CliLink::install(&f.runtime, &bin, &search, false).is_err());
        assert_eq!(
            CliLink::install(&f.runtime, &bin, &search, true).unwrap(),
            CliLink::Installed {
                path: bin.join("a")
            }
        );
    }
}
