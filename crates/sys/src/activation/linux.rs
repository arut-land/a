use super::{Command, Home, Listener, PathBuf, forget, installed, io, prepare, run};
use std::fmt::Write as _;
use std::os::fd::OwnedFd;
use std::path::Path;

pub(super) const SUPPORTED: bool = true;

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Registration {
    directory: PathBuf,
}

fn names(home: &Home) -> (String, String) {
    let identity = crate::endpoint::activation_identity(home);
    (
        format!("a-daemon-{identity:016x}.socket"),
        format!("a-daemon-{identity:016x}.service"),
    )
}

fn systemctl() -> Command {
    let mut command = Command::new("systemctl");
    command.arg("--user");
    command
}

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

fn socket_path(path: &Path) -> io::Result<String> {
    let value = path.to_str().ok_or(io::ErrorKind::InvalidInput)?;
    if !path.is_absolute() || value.chars().any(char::is_control) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // ListenStream accepts the literal address, unlike ExecStart's word list.
    Ok(value.replace('%', "%%"))
}

fn service(home: &Home, socket: &str) -> io::Result<String> {
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
    Ok(format!(
        "[Unit]\nDescription=a per-user agent daemon\nRequires={socket}\nAfter={socket}\n\n[Service]\nType=simple\nExecStart={} __daemon --activated\nWorkingDirectory=/\nUMask=0077\nKillMode=mixed\nTimeoutStopSec=30\n{settings}",
        quote(&home.runtime().current_exe())?.replace('$', "$$")
    ))
}

pub(super) fn install(
    home: &Home,
    app: bool,
    stop: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    if app {
        return Err(io::ErrorKind::Unsupported.into());
    }
    if installed(home)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "activation is already installed",
        ));
    }
    let (socket_name, service_name) = names(home);
    // This probes the actual user manager, including WSL/container deployments.
    run(systemctl().args(["show-environment"]))?;
    let endpoint = home.endpoint();
    crate::fs::private_dir(
        endpoint
            .path()
            .parent()
            .ok_or(io::ErrorKind::InvalidInput)?,
    )?;
    let socket = format!(
        "[Unit]\nDescription=a per-user agent socket\n\n[Socket]\nListenStream={}\nSocketMode=0600\nDirectoryMode=0700\nAccept=no\nRemoveOnStop=yes\n\n[Install]\nWantedBy=sockets.target\n",
        socket_path(endpoint.path())?
    );
    let service = service(home, &socket_name)?;
    let dir = units(home);
    std::fs::create_dir_all(&dir)?;
    let dir = std::fs::canonicalize(dir)?;
    for name in [&socket_name, &service_name] {
        if dir.join(name).exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "activation unit already exists",
            ));
        }
    }
    stop()?;
    crate::fs::write_private(&dir.join(&socket_name), socket.as_bytes())?;
    crate::fs::write_private(&dir.join(&service_name), service.as_bytes())?;
    prepare(
        home,
        &Registration {
            directory: dir.clone(),
        },
    )?;
    // Publish discovery before enabling: the first connect can enter the manager's backlog.
    crate::fs::write_private(
        &home.run_dir().join("daemon.endpoint"),
        endpoint.path().as_os_str().as_encoded_bytes(),
    )?;
    run(systemctl().arg("link").arg(dir.join(&service_name)))?;
    run(systemctl().arg("daemon-reload"))?;
    run(systemctl()
        .args(["enable", "--now"])
        .arg(dir.join(&socket_name)))
}

pub(super) fn uninstall(home: &Home, app: bool) -> io::Result<()> {
    if app {
        return Err(io::ErrorKind::Unsupported.into());
    }
    if !installed(home)? {
        return Ok(());
    }
    let Registration { directory } = Registration::read(home)?;
    let (socket_name, service_name) = names(home);
    // Stop the socket first, otherwise a reconnect could start another writer.
    run(systemctl().args(["disable", "--now", &socket_name]))?;
    run(systemctl().args(["stop", &service_name]))?;
    run(systemctl().args(["disable", &service_name]))?;
    for name in [&socket_name, &service_name] {
        std::fs::remove_file(directory.join(name))?;
    }
    run(systemctl().arg("daemon-reload"))?;
    forget(home)
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "launchd must await initial secure-socket discovery"
)]
pub(super) fn wait(_: &Home) -> io::Result<()> {
    Ok(())
}

pub(super) fn listener(home: &Home, executing_build: &str) -> io::Result<Listener> {
    super::current(home, executing_build)?;
    // listenfd also supports development supervisors. Admit only systemd's
    // PID-scoped, fd-3 contract, and bound its descriptor-vector allocation.
    let valid = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        == Some(std::process::id())
        && std::env::var("LISTEN_FDS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            == Some(1);
    // SAFETY: activation is consumed before runtime/child threads are started.
    unsafe {
        std::env::remove_var("LISTEN_FDS_FIRST_FD");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one activation socket for this process",
        ));
    }
    let mut descriptors = listenfd::ListenFd::from_env();
    if descriptors.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one systemd activation socket",
        ));
    }
    let socket = descriptors
        .take_unix_listener(0)?
        .ok_or(io::ErrorKind::InvalidInput)?;
    Listener::activated(std::fs::File::from(OwnedFd::from(socket)), home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_names_cannot_cross_state_homes() {
        let root = std::env::temp_dir().join(format!("a-activation-names-{}", std::process::id()));
        let one = Home::from(root.join("one"));
        let two = Home::from(root.join("two"));
        let (one_socket, one_service) = names(&one);
        let (two_socket, two_service) = names(&two);
        assert_ne!(one_socket, two_socket);
        assert_ne!(one_service, two_service);
    }

    #[test]
    fn unit_paths_cannot_expand_specifiers_or_inject_directives() {
        assert_eq!(
            quote(Path::new("/a %h/\"b\\c\n")).unwrap(),
            "\"/a %%h/\\\"b\\\\c\\n\""
        );
    }
}
