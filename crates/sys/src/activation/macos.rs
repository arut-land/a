use super::{Command, Home, Listener, PathBuf, forget, installed, io, prepare, run};
use std::os::fd::FromRawFd;

pub(super) const SUPPORTED: bool = true;

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) enum Registration {
    Launchd { plist: PathBuf },
    App,
}

const LABEL: &str = "dev.a.daemon";
fn label(home: &Home) -> String {
    format!(
        "{LABEL}.{:016x}",
        crate::endpoint::activation_identity(home)
    )
}

fn domain() -> String {
    format!("gui/{}", rustix::process::geteuid().as_raw())
}
fn plist(home: &Home) -> io::Result<PathBuf> {
    Ok(std::env::home_dir()
        .ok_or(io::ErrorKind::NotFound)?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(home))))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(super) fn install(
    home: &Home,
    app: bool,
    stop: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    if installed(home)? {
        if app && matches!(Registration::read(home)?, Registration::App) {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "activation is already installed",
        ));
    }
    if app {
        stop()?;
        return prepare(home, &Registration::App);
    }
    let path = plist(home)?;
    let label = label(home);
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
<key>Label</key><string>{label}</string>
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
    stop()?;
    crate::fs::write_private(&path, value.as_bytes())?;
    let path = std::fs::canonicalize(path)?;
    prepare(
        home,
        &Registration::Launchd {
            plist: path.clone(),
        },
    )?;
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
    match (Registration::read(home)?, app) {
        (Registration::Launchd { plist }, false) => {
            run(Command::new("launchctl").arg("bootout").arg(format!(
                "{}/{}",
                domain(),
                label(home)
            )))?;
            std::fs::remove_file(plist)?;
        }
        (Registration::App, true) => {}
        _ => {
            return Err(io::Error::other(
                "use the app to remove its background service, or the CLI for its LaunchAgent",
            ));
        }
    }
    forget(home)
}

pub(super) fn wait(home: &Home) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use std::time::{Duration, Instant};

    // Arm the directory before checking discovery: atomic publication replaces
    // the record inode, and a write between checking and waiting must wake us.
    let watch = crate::write_watch(&home.run_dir())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        watch.clear();
        if home.endpoint().connect().is_ok() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the LaunchAgent did not publish its socket; check Login Items approval and the daemon log",
            ));
        }
        let timeout = Timespec::try_from(remaining).map_err(io::Error::other)?;
        let mut fds = [PollFd::new(&watch, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

pub(super) fn listener(home: &Home, executing_build: &str) -> io::Result<Listener> {
    unsafe extern "C" {
        fn launch_activate_socket(
            name: *const libc::c_char,
            fds: *mut *mut libc::c_int,
            count: *mut libc::size_t,
        ) -> libc::c_int;
    }
    super::current(home, executing_build)?;
    let mut fds = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: the name is terminated and both output pointers are writable.
    let error = unsafe { launch_activate_socket(c"daemon".as_ptr(), &raw mut fds, &raw mut count) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    if count == 0 || fds.is_null() {
        // SAFETY: free accepts null or the allocation returned by launchd.
        unsafe { libc::free(fds.cast()) };
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launchd returned no socket",
        ));
    }
    // SAFETY: on success launchd returns count descriptors in a malloc allocation.
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
    fn registration_names_cannot_cross_state_homes() {
        let root = std::env::temp_dir().join(format!("a-activation-names-{}", std::process::id()));
        let one = Home::from(root.join("one"));
        let two = Home::from(root.join("two"));
        assert_ne!(label(&one), label(&two));
        assert_ne!(plist(&one).unwrap(), plist(&two).unwrap());
    }

    #[test]
    fn plist_values_cannot_become_markup() {
        assert_eq!(xml("<&\"'>"), "&lt;&amp;&quot;&apos;&gt;");
    }
}
