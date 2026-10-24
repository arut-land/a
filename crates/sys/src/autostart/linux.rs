use super::{Command, Home, io, run, update};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn name(home: &Home) -> String {
    format!(
        "a-daemon-{:016x}.service",
        crate::endpoint::home_identity(home)
    )
}

fn systemctl() -> Command {
    let mut command = Command::new("systemctl");
    command.arg("--user");
    command
}

/// `$XDG_CONFIG_HOME/systemd/user`. Enabled by path, so a user manager whose
/// own environment names another configuration base still finds it.
fn units(home: &Home) -> PathBuf {
    home.config_dir()
        .parent()
        .expect("native application directory")
        .join("systemd/user")
}

fn quote(path: &Path) -> io::Result<String> {
    let value = path.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "systemd paths must be UTF-8")
    })?;
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '%' => out.push_str("%%"),
            '\\' | '"' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            ch if ch.is_control() => return Err(io::ErrorKind::InvalidInput.into()),
            ch => out.push(ch),
        }
    }
    out.push('"');
    Ok(out)
}

fn unit(home: &Home) -> io::Result<String> {
    let mut env = Command::new("unused");
    home.daemon_env(&mut env);
    let mut settings = String::new();
    for (key, value) in env.get_envs() {
        if let Some(value) = value {
            let mut assignment = key.to_os_string();
            assignment.push("=");
            assignment.push(value);
            let _ = writeln!(settings, "Environment={}", quote(Path::new(&assignment))?);
        }
    }
    // Resolved XDG purposes travel in the unit, so the user manager's own
    // environment cannot select another home. No Restart=: a daemon that
    // exits (stopped, or drained for a newer build) is not brought back.
    Ok(format!(
        "[Unit]\nDescription=a per-user agent daemon\n\n[Service]\nType=simple\nExecStart={} __daemon --login\nWorkingDirectory=/\nUMask=0077\nKillMode=mixed\nTimeoutStopSec=30\n{settings}\n[Install]\nWantedBy=default.target\n",
        quote(&home.runtime().current_exe())?.replace('$', "$$")
    ))
}

pub(super) fn enable(home: &Home) -> io::Result<()> {
    // Probes the actual user manager, including WSL and container deployments.
    run(systemctl().arg("show-environment")).map_err(|_| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "no systemd user manager is running; a starts on first use",
        )
    })?;
    let path = units(home).join(name(home));
    update(&path, unit(home)?.as_bytes())?;
    run(systemctl().arg("daemon-reload"))?;
    run(systemctl().arg("enable").arg(path))
}

pub(super) fn disable(home: &Home) -> io::Result<()> {
    let name = name(home);
    let path = units(home).join(&name);
    if !path.exists() {
        return Ok(());
    }
    run(systemctl().arg("disable").arg(&name))?;
    std::fs::remove_file(path)?;
    run(systemctl().arg("daemon-reload"))
}

pub(super) fn refresh(home: &Home) -> io::Result<bool> {
    let name = name(home);
    let path = units(home).join(&name);
    if !path.exists() {
        return Ok(false);
    }
    if update(&path, unit(home)?.as_bytes())? {
        run(systemctl().arg("daemon-reload"))?;
    }
    // `systemctl --user disable` by hand is respected.
    Ok(run(systemctl().arg("is-enabled").arg(&name)).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_names_cannot_cross_state_homes() {
        let root = std::env::temp_dir().join(format!("a-autostart-names-{}", std::process::id()));
        assert_ne!(
            name(&Home::from(root.join("one"))),
            name(&Home::from(root.join("two")))
        );
    }

    #[test]
    fn unit_paths_cannot_expand_specifiers_or_inject_directives() {
        assert_eq!(
            quote(Path::new("/a %h/\"b\\c\n")).unwrap(),
            "\"/a %%h/\\\"b\\\\c\\n\""
        );
    }

    #[test]
    fn the_login_unit_starts_with_the_session_and_is_never_restarted() {
        let unit = unit(&Home::from(std::env::temp_dir().join("a-autostart-unit"))).unwrap();
        assert!(unit.contains(" __daemon --login\n"));
        assert!(unit.ends_with("[Install]\nWantedBy=default.target\n"));
        assert!(!unit.contains("Restart=") && !unit.contains("Sockets="));
    }
}
