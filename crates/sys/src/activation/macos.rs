use super::*;

pub(super) const SUPPORTED: bool = true;
use std::os::fd::FromRawFd;

const LABEL: &str = "dev.a.daemon";
const PLIST: &str = "dev.a.daemon.plist";

fn domain() -> String {
    format!("gui/{}", rustix::process::geteuid().as_raw())
}
fn plist() -> io::Result<PathBuf> {
    Ok(std::env::home_dir()
        .ok_or(io::ErrorKind::NotFound)?
        .join("Library/LaunchAgents")
        .join(PLIST))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(super) fn install(home: &Home, app: bool) -> io::Result<()> {
    if installed(home)? {
        if app && std::fs::read_to_string(record(home))? == "app" {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "activation is already installed",
        ));
    }
    if app {
        return prepare(home, "app");
    }
    let path = plist()?;
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "LaunchAgent already exists",
        ));
    }
    let exe = home.runtime().current_exe();
    let exe = exe.to_str().ok_or(io::ErrorKind::InvalidInput)?;
    let value = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{LABEL}</string>
<key>ProgramArguments</key><array><string>{}</string><string>__daemon</string><string>--activated</string></array>
<key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string></dict>
<key>WorkingDirectory</key><string>/</string>
<key>RunAtLoad</key><true/>
<key>Umask</key><integer>63</integer>
<key>ExitTimeOut</key><integer>30</integer>
<key>Sockets</key><dict><key>daemon</key><dict><key>SecureSocketWithKey</key><string>A_ACTIVATION_SOCKET</string><key>SockPathMode</key><integer>384</integer><key>SockType</key><string>stream</string><key>SockFamily</key><string>Unix</string></dict></dict>
</dict></plist>
"#,
        xml(exe),
        xml(std::env::home_dir()
            .ok_or(io::ErrorKind::NotFound)?
            .to_str()
            .ok_or(io::ErrorKind::InvalidInput)?)
    );
    std::fs::create_dir_all(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    crate::fs::write_private(&path, value.as_bytes())?;
    prepare(home, "launchd")?;
    let result = run(Command::new("launchctl")
        .arg("bootstrap")
        .arg(domain())
        .arg(&path));
    if result.is_err() {
        std::fs::remove_file(path)?;
        forget(home)?;
    }
    result
}

pub(super) fn uninstall(home: &Home, app: bool) -> io::Result<()> {
    if !installed(home)? {
        return Ok(());
    }
    let kind = std::fs::read_to_string(record(home))?;
    if (kind == "app") != app {
        return Err(io::Error::other(
            "use the app to remove its background service, or the CLI for its LaunchAgent",
        ));
    }
    if !app {
        run(Command::new("launchctl")
            .arg("bootout")
            .arg(format!("{}/{LABEL}", domain())))?;
        std::fs::remove_file(plist()?)?;
    }
    forget(home)
}

pub(super) fn listener(home: &Home) -> io::Result<Listener> {
    unsafe extern "C" {
        fn launch_activate_socket(
            name: *const libc::c_char,
            fds: *mut *mut libc::c_int,
            count: *mut libc::size_t,
        ) -> libc::c_int;
    }
    let mut fds = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: the name is terminated and both output pointers are writable.
    let error = unsafe { launch_activate_socket(c"daemon".as_ptr(), &raw mut fds, &raw mut count) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    // SAFETY: on success launchd returns count descriptors in a malloc allocation.
    if count == 0 || fds.is_null() {
        // SAFETY: free accepts null or the allocation returned by launchd.
        unsafe { libc::free(fds.cast()) };
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launchd returned no socket",
        ));
    }
    let files = unsafe {
        let files = std::slice::from_raw_parts(fds, count)
            .iter()
            .map(|&fd| std::fs::File::from_raw_fd(fd))
            .collect::<Vec<_>>();
        libc::free(fds.cast());
        files
    };
    if files.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one launchd activation socket",
        ));
    }
    // SAFETY: activation is consumed before runtime/child threads are started.
    unsafe { std::env::remove_var("A_ACTIVATION_SOCKET") };
    Listener::activated(files.into_iter().next().unwrap(), home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_values_cannot_become_markup() {
        assert_eq!(xml("<&\"'>"), "&lt;&amp;&quot;&apos;&gt;");
    }
}
