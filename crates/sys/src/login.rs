//! The environment the user's login shell sets up. An app opened from Finder
//! or the Dock inherits launchd's, whose `PATH` is only
//! `/usr/bin:/bin:/usr/sbin:/sbin`; the commands a session runs for it need
//! the user's own, the one their terminal has.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Brackets the variables in the shell's output, which may also carry
/// whatever its startup files print.
const MARK: &str = "A-LOGIN-ENV-7f3d";

/// Runs `shell` as an interactive login shell (`-l -i`, as editors do: on
/// macOS `PATH` additions often live in `.zshrc`, which only interactive
/// shells read) and returns the variables it exports. The shell gets a
/// session of its own and no terminal, so it can never stop for one, and
/// is killed with everything it started if it takes longer than `timeout`.
pub fn login_env(shell: &OsString, timeout: Duration) -> io::Result<Vec<(OsString, OsString)>> {
    let script = format!("printf %s {MARK}; /usr/bin/env -0; printf %s {MARK}");
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::new_session(&mut command);
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut out = Vec::new();
        let _ = tx.send(stdout.read_to_end(&mut out).map(|_| out));
    });
    let out = rx.recv_timeout(timeout);
    if out.is_err() {
        let _ = crate::kill_process_group(child.id());
    }
    let _ = child.wait();
    let out = out.map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{} took over {timeout:?} to start", shell.display()),
        )
    })??;
    parse(&out).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} printed no environment", shell.display()),
        )
    })
}

/// The `NAME=value` records between the first two marks.
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
                    (
                        OsString::from_vec(name.to_vec()),
                        OsString::from_vec(value.to_vec()),
                    )
                })
            })
            .collect(),
    )
}

#[cfg(test)]
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
