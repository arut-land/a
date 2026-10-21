//! `ssh <host> a bridge` with the system `ssh` (`A_SSH` overrides it, and
//! `A_SCP` overrides `scp`). Installs this build on a host that lacks `a`.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{ClientMsg, read_frame};
use sys::{Platform, Staged};

use crate::{Connection, Reader, Transport, Welcome, Writer};

/// What the bridge prints, with its token, once it is connected.
pub const SYNC: &str = "A-BRIDGE";

/// Ends a sync line followed by the host's environment, as a frame.
pub const WITH_ENV: &[u8] = b" env";

/// The exit status a POSIX shell gives a command it cannot find.
const NOT_FOUND: i32 = 127;

/// Opens a bridge to `host`'s daemon: the `a` on its `PATH`, else the one a
/// previous install staged, else this build after installing it there.
pub fn open(host: &str, staged: &Staged) -> io::Result<Connection> {
    let token = token();
    let found = find_script(&token);
    if let Some(conn) = bridge(host, &found, &token)? {
        return Ok(conn);
    }
    install(host, staged)?;
    let installed = format!(r#"exec "$HOME/.a/runtime/{}/a" bridge {token}"#, staged.id);
    bridge(host, &installed, &token)?
        .ok_or_else(|| io::Error::other(format!("the `a` installed on {host} does not run")))
}

pub(crate) fn find_script(token: &str) -> String {
    format!(
        r#"if command -v a >/dev/null 2>&1; then exec a bridge {token}; elif [ -x "$HOME/.a/runtime/current/a" ]; then exec "$HOME/.a/runtime/current/a" bridge {token}; else exit {NOT_FOUND}; fi"#
    )
}

/// Runs `script` on `host` and waits for the sync line; `None` if the shell
/// found no `a` to run.
fn bridge(host: &str, script: &str, token: &str) -> io::Result<Option<Connection>> {
    let mut connection = launch(host, script)?;
    let out = &mut connection.reader.0;
    let sync = format!("{SYNC} {token}");
    let mut line = Vec::new();
    let remote = loop {
        line.clear();
        if out.read_until(b'\n', &mut line)? == 0 {
            return match connection
                .writer
                .child
                .as_mut()
                .expect("bridge child")
                .wait()?
                .code()
            {
                Some(NOT_FOUND) => Ok(None),
                _ => Err(io::Error::other(format!(
                    "ssh {host}: the bridge ended before it was ready"
                ))),
            };
        }
        if line.trim_ascii_end().strip_prefix(sync.as_bytes()) == Some(WITH_ENV) {
            match read_frame(out)? {
                Some(ClientMsg::Environment(env)) => break env,
                _ => return Err(io::Error::other(format!("ssh {host}: no environment"))),
            }
        }
    };
    connection.remote = Some(remote);
    Ok(Some(connection))
}

pub(crate) fn launch(host: &str, script: &str) -> io::Result<Connection> {
    let (child, stdin, stdout) =
        sys::spawn_stdio(command("A_SSH", "ssh").arg(host).arg(sh(script)))?;
    Ok(Connection {
        reader: Reader(BufReader::new(Transport::Pipe(stdout))),
        writer: Writer {
            io: Some(Transport::Pipe(stdin)),
            child: Some(child),
        },
        remote: None,
        welcome: Welcome::default(),
        commands: Vec::new(),
    })
}

/// Copies this build to `~/.a/runtime/<id>/a` on `host`; a host of another
/// OS or architecture is refused.
pub(crate) fn install(host: &str, staged: &Staged) -> io::Result<()> {
    let uname = command("A_SSH", "ssh")
        .args([host, "uname -sm"])
        .stdin(Stdio::null())
        .output()?;
    let uname = String::from_utf8_lossy(&uname.stdout);
    let uname = uname.trim();
    let here = Platform::CURRENT;
    if Platform::from_uname(uname) != Some(here) {
        return Err(io::Error::other(format!(
            "`a` is not installed on {host}, and this build is for {here}, not {uname:?}; install `a` there and retry"
        )));
    }
    let dir = format!(".a/runtime/{}", staged.id);
    run(command("A_SSH", "ssh").args([host, &format!("mkdir -p '{dir}'")]))?;
    let mut scp = command("A_SCP", "scp");
    scp.arg("-q")
        .arg(&staged.exe)
        .arg(format!("{host}:{dir}/a"));
    run(&mut scp)
}

fn run(command: &mut Command) -> io::Result<()> {
    let status = command.stdin(Stdio::null()).status()?;
    if status.success() {
        Ok(())
    } else {
        // The command line as typed, not std's Debug of a whole Command.
        let line: Vec<_> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|word| word.to_string_lossy())
            .collect();
        Err(io::Error::other(format!(
            "`{}` failed: {status}",
            line.join(" ")
        )))
    }
}

fn command(var: &str, default: &str) -> Command {
    Command::new(std::env::var_os(var).unwrap_or_else(|| OsString::from(default)))
}

/// One argument for the remote login shell, which may not be POSIX: it only
/// has to start `sh` with a single-quoted script.
fn sh(script: &str) -> String {
    format!("sh -c '{}'", script.replace('\'', r"'\''"))
}

/// Distinguishes our sync line from anything a login shell could print.
pub(crate) fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{:x}{:x}", nanos, std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_scripts_for_the_login_shell() {
        assert_eq!(sh("echo 'hi'"), r"sh -c 'echo '\''hi'\'''");
    }
}
