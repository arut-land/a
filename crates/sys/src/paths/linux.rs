use super::{Command, InstallLayout, Locations, PathBuf, Paths, io};

pub(super) fn resolve() -> io::Result<Paths> {
    resolve_at(std::env::home_dir, |key| {
        std::env::var_os(key).map(PathBuf::from)
    })
}

fn resolve_at(
    home: impl FnOnce() -> Option<PathBuf>,
    env: impl Fn(&str) -> Option<PathBuf>,
) -> io::Result<Paths> {
    let home = std::cell::LazyCell::new(home);
    let base = |key: &str, fallback: &str| -> io::Result<PathBuf> {
        env(key)
            .filter(|path| path.is_absolute())
            .or_else(|| home.as_ref().map(|home| home.join(fallback)))
            .map(|base| base.join("a"))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "no home directory for XDG default")
            })
    };
    let state = base("XDG_STATE_HOME", ".local/state")?;
    let runtime = crate::endpoint::runtime_directory();
    Ok(Paths::new(Locations {
        layout: InstallLayout::Native,
        config: base("XDG_CONFIG_HOME", ".config")?,
        data: base("XDG_DATA_HOME", ".local/share")?,
        cache: base("XDG_CACHE_HOME", ".cache")?,
        logs: state.join("logs"),
        state,
        temporary: runtime.join("a-tmp"),
        runtime,
    }))
}

pub(super) fn daemon_env(paths: &Paths, command: &mut Command) {
    for (key, dir) in [
        ("XDG_CONFIG_HOME", &paths.inner.config),
        ("XDG_DATA_HOME", &paths.inner.data),
        ("XDG_STATE_HOME", &paths.inner.state),
        ("XDG_CACHE_HOME", &paths.inner.cache),
    ] {
        command.env(key, dir.parent().expect("application subdirectory"));
    }
    command.env("XDG_RUNTIME_DIR", &paths.inner.runtime);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn relative_xdg_values_never_make_paths_depend_on_cwd() {
        let home = Path::new("/home/example");
        let paths = resolve_at(|| Some(home.to_owned()), |_| Some("relative".into())).unwrap();
        assert_eq!(paths.config_dir(), home.join(".config/a"));
        assert_eq!(paths.state_dir(), home.join(".local/state/a"));
        assert_eq!(paths.data_dir(), home.join(".local/share/a"));
        assert_eq!(paths.cache_dir(), home.join(".cache/a"));
    }

    #[test]
    fn a_spawned_native_daemon_keeps_every_resolved_directory_purpose() {
        let paths = resolve_at(
            || Some("/unused".into()),
            |key| Some(PathBuf::from(format!("/separate/{key}"))),
        )
        .unwrap();
        let mut command = Command::new("a");
        paths.daemon_env(&mut command);
        let env = |key: &str| {
            command
                .get_envs()
                .find(|(k, _)| *k == key)
                .and_then(|(_, v)| v)
                .map(PathBuf::from)
        };
        let child = resolve_at(|| Some("/different/home".into()), env).unwrap();
        assert_eq!(child.config_dir(), paths.config_dir());
        assert_eq!(child.data_dir(), paths.data_dir());
        assert_eq!(child.state_dir(), paths.state_dir());
        assert_eq!(child.cache_dir(), paths.cache_dir());
        assert!(
            command
                .get_envs()
                .any(|(k, v)| k == "A_HOME" && v.is_none())
        );
    }

    #[test]
    fn writable_state_is_independent_of_a_read_only_or_network_home() {
        let paths = resolve_at(
            || Some("/network/read-only".into()),
            |key| match key {
                "XDG_STATE_HOME" => Some("/local/state".into()),
                "XDG_CACHE_HOME" => Some("/local/cache".into()),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(paths.sessions_dir(), Path::new("/local/state/a/sessions"));
        assert_eq!(paths.spill_dir(), Path::new("/local/state/a/spill"));
        assert_eq!(paths.cache_dir(), Path::new("/local/cache/a"));
        assert!(
            paths
                .runtime()
                .dir()
                .starts_with("/network/read-only/.local/share/a")
        );
    }

    #[test]
    fn complete_absolute_xdg_overrides_work_without_a_home_lookup() {
        let lookups = std::cell::Cell::new(0);
        let paths = resolve_at(
            || {
                lookups.set(lookups.get() + 1);
                None
            },
            |key| Some(PathBuf::from(format!("/local/{key}"))),
        )
        .unwrap();
        assert_eq!(lookups.get(), 0);
        assert_eq!(paths.config_dir(), Path::new("/local/XDG_CONFIG_HOME/a"));
        assert_eq!(paths.data_dir(), Path::new("/local/XDG_DATA_HOME/a"));
        assert_eq!(paths.state_dir(), Path::new("/local/XDG_STATE_HOME/a"));
        assert_eq!(paths.cache_dir(), Path::new("/local/XDG_CACHE_HOME/a"));
    }

    #[test]
    fn a_missing_xdg_default_without_a_home_is_an_error() {
        let error = resolve_at(|| None, |_| None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
