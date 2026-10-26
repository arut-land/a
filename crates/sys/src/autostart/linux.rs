use super::{Command, Home, Launch, io, run, update};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub(super) const LOGIN: super::Login = super::Login::Serve;

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
    // The user manager provides its own XDG_RUNTIME_DIR, the one the
    // session's clients use; a baked copy would go stale across boots.
    for (key, value) in env.get_envs().filter(|(key, _)| *key != "XDG_RUNTIME_DIR") {
        if let Some(value) = value {
            let mut assignment = key.to_os_string();
            assignment.push("=");
            assignment.push(value);
            let _ = writeln!(settings, "Environment={}", quote(Path::new(&assignment))?);
        }
    }
    // Resolved XDG purposes travel in the unit, so the user manager's own
    // environment cannot select another home. No Restart=: a daemon that
    // exits is started again only by a client or the next login.
    Ok(format!(
        "[Unit]\nDescription=a per-user agent daemon\n\n[Service]\nType=notify\nNotifyAccess=main\nExecStart={} __daemon --login\nWorkingDirectory=/\nUMask=0077\nKillMode=mixed\nTimeoutStopSec=30\n{settings}\n[Install]\nWantedBy=default.target\n",
        quote(&home.runtime().current_exe())?.replace('$', "$$")
    ))
}

pub(super) fn start(home: &Home, launch: Launch) -> io::Result<bool> {
    start_registered(home, launch, &units(home).join(name(home)), run)
}

fn start_registered(
    home: &Home,
    launch: Launch,
    path: &Path,
    mut run: impl FnMut(&mut Command) -> io::Result<()>,
) -> io::Result<bool> {
    if !path.try_exists()? {
        return Ok(false);
    }
    if update(path, unit(home)?.as_bytes())? {
        run(systemctl().arg("daemon-reload"))?;
    }
    if launch != Launch::Start {
        crate::crash::reset(home)?;
        run(systemctl().arg("reset-failed").arg(name(home)))?;
    }
    run(systemctl()
        .arg(if launch == Launch::Replace {
            "restart"
        } else {
            "start"
        })
        .arg(name(home)))?;
    Ok(true)
}

pub(super) fn ready() -> io::Result<()> {
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    notify(&socket)
}

fn notify(socket: &std::ffi::OsStr) -> io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let address = match socket.as_bytes().strip_prefix(b"@") {
        Some(name) => SocketAddr::from_abstract_name(name)?,
        None => SocketAddr::from_pathname(socket)?,
    };
    UnixDatagram::unbound()?.send_to_addr(b"READY=1", &address)?;
    Ok(())
}

pub(super) fn registration(home: &Home) -> io::Result<super::Registration> {
    if !units(home).join(name(home)).try_exists()? {
        return Ok(super::Registration::Unregistered);
    }
    Ok(
        if run(systemctl().arg("is-enabled").arg(name(home))).is_ok() {
            super::Registration::Enabled
        } else {
            super::Registration::Disabled
        },
    )
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
    #[cfg_attr(miri, ignore = "Miri does not support AF_UNIX sockets")]
    fn readiness_reaches_both_native_notify_socket_names() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixDatagram};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notify");
        let abstract_name = format!("a-notify-{}", std::process::id());
        let abstract_socket = format!("@{abstract_name}");
        for (socket, address) in [
            (path.as_os_str(), SocketAddr::from_pathname(&path).unwrap()),
            (
                std::ffi::OsStr::new(&abstract_socket),
                SocketAddr::from_abstract_name(abstract_name.as_bytes()).unwrap(),
            ),
        ] {
            let receiver = UnixDatagram::bind_addr(&address).unwrap();
            receiver
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            notify(socket).unwrap();
            let mut bytes = [0; 64];
            let count = receiver.recv(&mut bytes).unwrap();
            assert_eq!(&bytes[..count], b"READY=1");
        }
    }

    #[test]
    fn registered_replacements_stay_owned_by_the_user_unit() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::from(root.path().join("home"));
        std::fs::create_dir_all(home.state_dir()).unwrap();
        let path = root.path().join("daemon.service");
        std::fs::write(&path, "outdated unit").unwrap();
        let mut commands = Vec::new();
        assert!(
            start_registered(&home, Launch::Replace, &path, |command| {
                commands.push(
                    command
                        .get_args()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                );
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(
            commands,
            [
                vec!["--user".to_owned(), "daemon-reload".to_owned()],
                vec!["--user".to_owned(), "reset-failed".to_owned(), name(&home)],
                vec!["--user".to_owned(), "restart".to_owned(), name(&home)]
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            unit(&home).unwrap()
        );
        assert!(!home.run_dir().join("daemon.endpoint").exists());
        assert!(
            !start_registered(
                &home,
                Launch::Start,
                &root.path().join("missing"),
                |_| panic!("unmanaged installation invoked systemd")
            )
            .unwrap()
        );
        assert!(
            start_registered(&home, Launch::Start, &path, |_| Err(io::Error::other(
                "manager refused"
            )))
            .is_err()
        );
    }

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
        assert!(unit.contains("Type=notify\nNotifyAccess=main\n"));
        assert!(unit.ends_with("[Install]\nWantedBy=default.target\n"));
        assert!(!unit.contains("Restart=") && !unit.contains("Sockets="));
        assert!(!unit.contains("XDG_RUNTIME_DIR"));
    }
}
