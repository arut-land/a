use super::{Command, InstallLayout, Locations, Path, PathBuf, Paths, io};

fn base(value: Option<PathBuf>, home: &Path, fallback: &str) -> PathBuf {
    value
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(fallback))
        .join("a")
}

pub(super) fn resolve() -> io::Result<Paths> {
    let home = std::env::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
    Ok(resolve_at(&home, |key| {
        std::env::var_os(key).map(PathBuf::from)
    }))
}

fn resolve_at(home: &Path, env: impl Fn(&str) -> Option<PathBuf>) -> Paths {
    let state = base(env("XDG_STATE_HOME"), home, ".local/state");
    let runtime = crate::endpoint::runtime_directory();
    Paths::new(Locations {
        layout: InstallLayout::Native,
        config: base(env("XDG_CONFIG_HOME"), home, ".config"),
        data: base(env("XDG_DATA_HOME"), home, ".local/share"),
        cache: base(env("XDG_CACHE_HOME"), home, ".cache"),
        logs: state.join("logs"),
        state,
        temporary: runtime.join("a-tmp"),
        runtime,
    })
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

pub(super) fn remote_runtime_script() -> &'static str {
    r#"case "$XDG_DATA_HOME" in
/*) a_runtime="$XDG_DATA_HOME/a/runtime" ;;
*) a_runtime="$HOME/.local/share/a/runtime" ;;
esac"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_xdg_values_never_make_paths_depend_on_cwd() {
        let home = Path::new("/home/example");
        let paths = resolve_at(home, |_| Some("relative".into()));
        assert_eq!(paths.config_dir(), home.join(".config/a"));
        assert_eq!(paths.state_dir(), home.join(".local/state/a"));
        assert_eq!(paths.data_dir(), home.join(".local/share/a"));
        assert_eq!(paths.cache_dir(), home.join(".cache/a"));
    }

    #[test]
    fn a_spawned_native_daemon_keeps_every_resolved_directory_purpose() {
        let paths = resolve_at(Path::new("/unused"), |key| {
            Some(PathBuf::from(format!("/separate/{key}")))
        });
        let mut command = Command::new("a");
        paths.daemon_env(&mut command);
        let env = |key: &str| {
            command
                .get_envs()
                .find(|(k, _)| *k == key)
                .and_then(|(_, v)| v)
                .map(PathBuf::from)
        };
        let child = resolve_at(Path::new("/different/home"), env);
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
        let paths = resolve_at(Path::new("/network/read-only"), |key| match key {
            "XDG_STATE_HOME" => Some("/local/state".into()),
            "XDG_CACHE_HOME" => Some("/local/cache".into()),
            _ => None,
        });
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
}
