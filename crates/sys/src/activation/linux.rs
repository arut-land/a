use super::*;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::path::Path;

const SOCKET: &str = "a-daemon.socket";
const SERVICE: &str = "a-daemon.service";

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

fn service(home: &Home) -> io::Result<String> {
    let mut env = Command::new("unused");
    home.daemon_env(&mut env);
    let mut settings = String::new();
    for (key, value) in env.get_envs() {
        if let Some(value) = value {
            settings.push_str(&format!(
                "Environment={}\n",
                quote(Path::new(&format!(
                    "{}={}",
                    key.to_string_lossy(),
                    value.to_string_lossy()
                )))?
            ));
        }
    }
    Ok(format!(
        "[Unit]\nDescription=a per-user agent daemon\nRequires={SOCKET}\nAfter={SOCKET}\n\n[Service]\nType=simple\nExecStart={} __daemon --activated\nWorkingDirectory=/\nUMask=0077\nKillMode=mixed\nTimeoutStopSec=30\n{settings}",
        quote(&home.runtime().current_exe())?.replace('$', "$$")
    ))
}

pub(super) fn install(home: &Home, app: bool) -> io::Result<()> {
    if app {
        return Err(io::ErrorKind::Unsupported.into());
    }
    if installed(home)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "activation is already installed",
        ));
    }
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
        "[Unit]\nDescription=a per-user agent socket\n\n[Socket]\nListenStream={}\nSocketMode=0600\nAccept=no\nRemoveOnStop=yes\n\n[Install]\nWantedBy=sockets.target\n",
        quote(endpoint.path())?
    );
    let service = service(home)?;
    let dir = units(home);
    std::fs::create_dir_all(&dir)?;
    for name in [SOCKET, SERVICE] {
        if dir.join(name).exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "activation unit already exists",
            ));
        }
    }
    crate::fs::write_private(&dir.join(SOCKET), socket.as_bytes())?;
    crate::fs::write_private(&dir.join(SERVICE), service.as_bytes())?;
    prepare(home, "systemd")?;
    // Publish discovery before enabling: the first connect can enter the manager's backlog.
    crate::fs::write_private(
        &home.run_dir().join("daemon.endpoint"),
        endpoint.path().as_os_str().as_encoded_bytes(),
    )?;
    run(systemctl().arg("link").arg(dir.join(SERVICE)))?;
    run(systemctl().arg("daemon-reload"))?;
    run(systemctl().args(["enable", "--now"]).arg(dir.join(SOCKET)))
}

pub(super) fn uninstall(home: &Home, app: bool) -> io::Result<()> {
    if app {
        return Err(io::ErrorKind::Unsupported.into());
    }
    if !installed(home)? {
        return Ok(());
    }
    // Stop the socket first, otherwise a reconnect could start another writer.
    run(systemctl().args(["disable", "--now", SOCKET]))?;
    run(systemctl().args(["stop", SERVICE]))?;
    run(systemctl().args(["disable", SERVICE]))?;
    for name in [SOCKET, SERVICE] {
        std::fs::remove_file(units(home).join(name))?;
    }
    run(systemctl().arg("daemon-reload"))?;
    forget(home)
}

pub(super) fn listener(home: &Home) -> io::Result<Listener> {
    let mut descriptors =
        libsystemd::activation::receive_descriptors(true).map_err(io::Error::other)?;
    if descriptors.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one systemd activation socket",
        ));
    }
    let fd = descriptors.pop().unwrap().into_raw_fd();
    // SAFETY: receive_descriptors transfers this inherited descriptor to us.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    Listener::activated(file, home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_paths_cannot_expand_specifiers_or_inject_directives() {
        assert_eq!(
            quote(Path::new("/a %h/\"b\\c\n")).unwrap(),
            "\"/a %%h/\\\"b\\\\c\\n\""
        );
    }
}
