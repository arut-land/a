//! The environment the user's login shell sets up. An app opened from Finder
//! or the Dock inherits launchd's, whose `PATH` is only
//! `/usr/bin:/bin:/usr/sbin:/sbin`; the commands a session runs for it need
//! the user's own, the one their terminal has. Windows apps inherit the
//! user's environment as it is, so there is nothing to capture there.

use std::ffi::OsString;
use std::io;
use std::time::Duration;

/// Brackets the variables in the shell's output, which may also carry
/// whatever its startup files print.
#[cfg(unix)]
const MARK: &str = "A-LOGIN-ENV-7f3d";

/// Runs `shell` as an interactive login shell (`-l -i`, as editors do: on
/// macOS `PATH` additions often live in `.zshrc`, which only interactive
/// shells read) and returns the variables it exports. The shell gets a
/// session of its own and no terminal, so it can never stop for one, and
/// is killed with everything it started if it takes longer than `timeout`.
#[cfg(unix)]
pub fn login_env(shell: &OsString, timeout: Duration) -> io::Result<Vec<(OsString, OsString)>> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    struct Capture(std::process::Child, crate::OwnedGroup);
    impl Drop for Capture {
        fn drop(&mut self) {
            // Every error path (including unwinding) revokes the entire
            // shell group and reaps the child. No detached reader survives.
            let _ = self.1.signal(crate::Signal::Kill);
            let _ = self.0.wait();
        }
    }

    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(io::ErrorKind::InvalidInput)?;
    let script = format!("printf %s {MARK}; /usr/bin/env -0; printf %s {MARK}");
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::OwnedGroup::lead(&mut command);
    let child = crate::proc::spawn(&mut command)?;
    let group = crate::OwnedGroup::new(&child);
    let mut child = Capture(child, group);
    let mut stdout = child.0.stdout.take().expect("stdout is piped");
    fcntl_setfl(&stdout, fcntl_getfl(&stdout)? | OFlags::NONBLOCK)?;
    let mut out = Vec::new();
    let mut eof = false;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{} took over {timeout:?} to start", shell.display()),
                )
            })?;
        if !eof {
            let mut buf = [0u8; 8192];
            match stdout.read(&mut buf) {
                Ok(0) => eof = true,
                Ok(n) => {
                    if out.len() + n > 8 * 1024 * 1024 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "login output exceeds 8 MiB",
                        ));
                    }
                    out.extend_from_slice(&buf[..n]);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        // Keep the zombie until group cleanup, so its pgid cannot be reused.
        let exited = match waitid(
            WaitId::Pid(
                Pid::from_raw(child.0.id().try_into().expect("child pid fits i32")).unwrap(),
            ),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(status) => status.is_some(),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        };
        if eof && exited {
            break;
        }
        // Pipe EOF does not imply process exit. Poll in bounded slices so
        // either event is covered by the same deadline, without a reader thread.
        let slice = remaining.min(Duration::from_millis(10));
        let wait = Timespec {
            tv_sec: 0,
            tv_nsec: i64::from(slice.subsec_nanos()),
        };
        let mut fds = [PollFd::new(&stdout, PollFlags::IN)];
        let fds = if eof { &mut [][..] } else { &mut fds[..] };
        match poll(fds, Some(&wait)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
    parse(&out).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} printed no environment", shell.display()),
        )
    })
}

/// Nothing to capture: see the module's documentation.
#[cfg(windows)]
pub fn login_env(_shell: &OsString, _timeout: Duration) -> io::Result<Vec<(OsString, OsString)>> {
    Err(io::ErrorKind::Unsupported.into())
}

/// The `NAME=value` records between the first two marks.
#[cfg(unix)]
fn parse(out: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    let mark = MARK.as_bytes();
    let start = out.windows(mark.len()).position(|w| w == mark)? + mark.len();
    let len = out[start..].windows(mark.len()).position(|w| w == mark)?;
    Some(
        out[start..start + len]
            .split(|&b| b == 0)
            .filter_map(|record| {
                let eq = record.iter().position(|&b| b == b'=')?;
                let (name, value) = (&record[..eq], &record[eq + 1..]);
                (!name.is_empty()).then(|| {
                    use std::os::unix::ffi::OsStringExt;
                    (
                        OsString::from_vec(name.to_vec()),
                        OsString::from_vec(value.to_vec()),
                    )
                })
            })
            .collect(),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn parses_between_the_marks_past_any_noise() {
        let out = format!("welcome!\n{MARK}PATH=/a:/b\0EMPTY=\0MULTI=x\ny=z\0{MARK}bye");
        let vars = parse(out.as_bytes()).unwrap();
        let names: Vec<_> = vars
            .iter()
            .map(|(k, v)| (k.to_str().unwrap(), v.to_str().unwrap()))
            .collect();
        assert_eq!(
            names,
            [("PATH", "/a:/b"), ("EMPTY", ""), ("MULTI", "x\ny=z")]
        );
        assert!(parse(b"no marks").is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn login_deadline_includes_child_exit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("a-login-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shell");
        // The output is complete and EOF arrives immediately; the shell
        // stays alive until deadline cleanup kills and reaps its group.
        std::fs::write(
            &path,
            "#!/bin/sh\necho $$ > \"$0.pid\"\neval \"$4\"\nexec 1>&-\nexec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let result = login_env(&path.as_os_str().to_os_string(), Duration::from_secs(1));
        let pid: i32 = std::fs::read_to_string(path.with_extension("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let reaped = rustix::process::waitpid(
            rustix::process::Pid::from_raw(pid),
            rustix::process::WaitOptions::NOHANG,
        );
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(reaped.unwrap_err(), rustix::io::Errno::CHILD);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn asks_a_shell_and_gives_up_on_a_slow_one() {
        let dir = std::env::temp_dir().join(format!("a-login-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A "shell" that ignores its flags: one sets a variable and runs
        // the script, the other hangs.
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            OsString::from(path)
        };
        let good = script("good", "echo noise; export FROM_LOGIN=yes; eval \"$4\"");
        let vars = login_env(&good, Duration::from_secs(10)).unwrap();
        assert!(vars.iter().any(|(k, v)| k == "FROM_LOGIN" && v == "yes"));

        let slow = script("slow", "sleep 30");
        let err = login_env(&slow, Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let inherited = script("inherited", "sleep 30 & eval \"$4\"");
        assert_eq!(
            login_env(&inherited, Duration::from_millis(100))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let noisy = script("noisy", "exec /usr/bin/yes");
        assert_eq!(
            login_env(&noisy, Duration::from_secs(5))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
