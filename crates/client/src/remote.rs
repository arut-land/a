//! `ssh <host> a bridge` with the system `ssh` (`A_SSH` overrides it).
//! Installs this build on a host that lacks `a`, streaming its bytes over SSH.

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
    let installed = installed_script(&staged.id, &token);
    bridge(host, &installed, &token)?
        .ok_or_else(|| io::Error::other(format!("the `a` installed on {host} does not run")))
}

pub(crate) fn find_script(token: &str) -> String {
    format!(
        r#"if command -v a >/dev/null 2>&1; then exec a bridge {token}; fi
{}
if [ -x "$a_runtime/current/a" ]; then exec "$a_runtime/current/a" bridge {token}; else exit {NOT_FOUND}; fi"#,
        sys::RemoteLayout::discovery_script()
    )
}

pub(crate) fn installed_script(id: &str, token: &str) -> String {
    format!(
        "{}\nexec \"$a_runtime/{id}/a\" bridge {token}",
        sys::RemoteLayout::for_platform(Platform::CURRENT).runtime_script()
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
    let (child, stdin, stdout) = sys::spawn_ssh(command("A_SSH", "ssh").arg(host).arg(sh(script)))?;
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

/// Copies this build to the host layout's `runtime/<id>/a`; a host of another
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
    let (crc, size) = cksum(std::fs::File::open(&staged.exe)?)?;
    // An image that does not match is a crash leftover from an interrupted
    // upload, never a build to run.
    let verify = format!(
        r#"crc={crc} size={size}
verify() {{ [ -x "$1" ] && set -- $(cksum < "$1") && [ "$1 $2" = "$crc $size" ]; }}"#
    );
    let prepare = format!(
        r#"{}
{verify}
if verify "$a_runtime/{id}/a"; then exit 0; fi
umask 077
mkdir -p "$a_runtime/{id}" && mktemp -d "$a_runtime/{id}/.install.XXXXXXXXXX""#,
        sys::RemoteLayout::for_platform(here).runtime_script(),
        id = staged.id,
    );
    let prepared = command("A_SSH", "ssh")
        .arg(host)
        .arg(sh(&prepare))
        .stdin(Stdio::null())
        .output()?;
    if !prepared.status.success() {
        return Err(io::Error::other(format!(
            "ssh {host}: cannot prepare runtime directory: {}",
            String::from_utf8_lossy(&prepared.stderr).trim_end()
        )));
    }
    if prepared.stdout.is_empty() {
        return Ok(());
    }
    let dir = String::from_utf8(prepared.stdout)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let dir = dir.strip_suffix('\n').ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "ssh: missing runtime directory")
    })?;
    // A cut connection ends `cat` with a clean EOF, so only the checksum
    // proves the upload whole. It is flushed before its name exists, and a
    // concurrent installer may win, but a verified image is never replaced.
    let upload = format!(
        "{} -- '{}'",
        sh(&format!(
            r#"set -u
umask 077
upload=$1
{verify}
trap 'rm -f "$upload/a"; rmdir "$upload"' 0
trap 'exit 1' HUP INT TERM
cat > "$upload/a" && chmod 700 "$upload/a" && verify "$upload/a" && sync || exit 1
ln "$upload/a" "$upload/../a" 2>/dev/null || verify "$upload/../a" || mv -f "$upload/a" "$upload/../a""#
        )),
        dir.replace('\'', r"'\''")
    );
    let status = command("A_SSH", "ssh")
        .arg(host)
        .arg(upload)
        .stdin(std::fs::File::open(&staged.exe)?)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ssh {host}: upload failed: {status}"
        )))
    }
}

/// POSIX `cksum` of a file: its CRC and its length.
fn cksum(mut file: impl io::Read) -> io::Result<(u32, u64)> {
    const CKSUM: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_CKSUM);
    let mut digest = CKSUM.digest();
    let mut buf = vec![0; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
        size += n as u64;
    }
    let mut len = size;
    while len > 0 {
        digest.update(&len.to_le_bytes()[..1]);
        len >>= 8;
    }
    Ok((digest.finalize(), size))
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

    #[cfg(unix)]
    #[test]
    fn upload_checksums_match_the_hosts_cksum() {
        for len in [0, 1, 255, 256, 65_537, 200_003] {
            let bytes: Vec<u8> = (0..len).map(|n: u32| (n * 31 % 251) as u8).collect();
            let mut child = Command::new("cksum")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            io::Write::write_all(&mut child.stdin.take().unwrap(), &bytes).unwrap();
            let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
            let (crc, size) = cksum(&bytes[..]).unwrap();
            assert_eq!(
                out.split_whitespace().collect::<Vec<_>>(),
                [crc.to_string(), size.to_string()]
            );
        }
    }

    #[test]
    fn quotes_scripts_for_the_login_shell() {
        assert_eq!(sh("echo 'hi'"), r"sh -c 'echo '\''hi'\'''");
    }
}
