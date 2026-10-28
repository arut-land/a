//! Another machine's daemon, through the system `ssh` (`A_SSH` overrides
//! it) to `a bridge` there, whose stdio carries the frames. The remote
//! login shell is given only plain words, which any shell parses alike: a
//! POSIX host (whatever its login shell, csh and fish included) runs
//! `sh -s` with the script on stdin; one without `sh` (cmd or PowerShell
//! on Windows) is asked for `a bridge <token>`, which serves where its `a`
//! is on PATH. Installs this build on a POSIX host that lacks `a`,
//! streaming its bytes over SSH.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::{ClientMsg, MachineId};
use rt::{BoxFuture, Rt};
use sys::{Home, Platform, Prompt, Staged};

use crate::link::{Link, LinkError, Opened, Pipe, Start, Stream};
use crate::{Error, Host};

/// What the bridge prints, with its token, once it is connected.
pub const SYNC: &str = "A-BRIDGE";

/// What a script says first, with its token: a POSIX shell runs it.
const SHELL: &str = "A-SH";

/// Ends a sync line followed by the host's environment, as a frame.
pub const WITH_ENV: &[u8] = b" env";

/// The exit status a POSIX shell gives a command it cannot find, and the
/// discovery script gives when it finds no `a`.
const NOT_FOUND: i32 = 127;

/// The exit status of `ssh` itself failing: it could not connect or
/// authenticate.
const SSH_FAILED: i32 = 255;

/// Another machine's daemon over `ssh <host>`: the `a` on its `PATH`, else
/// the one a previous install staged, else this build after installing it
/// there. `home` is this machine's, whose machine id the hello names.
#[derive(Clone, Debug)]
pub struct SshLink {
    host: Host,
    prompt: Prompt,
    home: Home,
}

impl SshLink {
    pub const fn new(host: Host, prompt: Prompt, home: Home) -> Self {
        Self { host, prompt, home }
    }
}

impl Link for SshLink {
    fn open<'a>(
        &'a self,
        rt: &'a Rt,
        build: &'a Staged,
        start: Start,
    ) -> BoxFuture<'a, Result<Opened, Error>> {
        Box::pin(async move {
            if start == Start::Replace {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "this command acts on this machine; run it on {} instead",
                        self.host
                    ),
                )
                .into());
            }
            let home = self.home.clone();
            let machine = rt::try_unblock_with(
                &**rt,
                rt::BlockingClass::Storage,
                rt::STORAGE_BYTES,
                move || sys::machine_id(&home),
            )
            .await??;
            let opened = |opened: Opened| Opened {
                machine: Some(MachineId(machine)),
                ..opened
            };
            let token = token();
            match self.bridge(rt, Bootstrap::Find, &token).await? {
                Bridge::Ready(ready) => return Ok(opened(ready)),
                Bridge::Ended(ended) if ended.shell && ended.code == Some(NOT_FOUND) => {}
                Bridge::Ended(ended) if ended.shell => return Err(self.ended(&ended)),
                // No POSIX shell ran discovery (a Windows host, say): only an
                // `a` its login shell finds can serve.
                Bridge::Ended(_) => {
                    return match self.bridge(rt, Bootstrap::Direct, &token).await? {
                        Bridge::Ready(ready) => Ok(opened(ready)),
                        Bridge::Ended(ended) => Err(self.ended(&ended)),
                    };
                }
            }
            let (link, installing) = (self.clone(), build.clone());
            rt::try_unblock_with(&**rt, rt::BlockingClass::Storage, 64 << 10, move || {
                link.install(&installing)
            })
            .await??;
            match self
                .bridge(rt, Bootstrap::Installed(&build.id), &token)
                .await?
            {
                Bridge::Ready(ready) => Ok(opened(ready)),
                Bridge::Ended(_) => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("the `a` installed on {} does not run", self.host),
                )
                .into()),
            }
        })
    }

    fn needs_terminal(&self) -> bool {
        self.prompt == Prompt::Terminal
    }

    fn mismatch(&self, _: &Staged, theirs: u32) -> Error {
        Error::Mismatch {
            theirs,
            newer: None,
            host: Some(self.host.clone()),
        }
    }
}

/// How a bridge is started on the host.
#[derive(Clone, Copy)]
enum Bootstrap<'a> {
    /// The `a` on its PATH, else the one its runtime directory holds.
    Find,
    /// The `a` its login shell finds, with no POSIX shell to help.
    Direct,
    /// The build installed under this id.
    Installed(&'a str),
}

enum Bridge {
    Ready(Opened),
    Ended(Ended),
}

/// A bridge that ended before it was ready.
struct Ended {
    /// Whether a POSIX shell ran the script: it says so first.
    shell: bool,
    code: Option<i32>,
    /// What it said on stderr, where that is not the person's terminal.
    said: String,
}

impl SshLink {
    fn ssh(&self) -> Command {
        let mut cmd =
            Command::new(std::env::var_os("A_SSH").unwrap_or_else(|| OsString::from("ssh")));
        if self.prompt == Prompt::Never {
            cmd.args(["-o", "BatchMode=yes"]);
        }
        cmd.arg(&self.host);
        cmd
    }

    /// Why the bridge `ended`, as the error to report.
    fn ended(&self, ended: &Ended) -> Error {
        let status = ended
            .code
            .map_or_else(|| "a signal".to_owned(), |code| format!("status {code}"));
        let said = if ended.said.is_empty() {
            String::new()
        } else {
            format!(": {}", ended.said)
        };
        io::Error::other(format!(
            "ssh {}: the bridge ended with {status} before it was ready{said}",
            self.host
        ))
        .into()
    }

    /// Starts the bridge as `how` says and waits for its sync line.
    async fn bridge(&self, rt: &Rt, how: Bootstrap<'_>, token: &str) -> Result<Bridge, Error> {
        let mut cmd = self.ssh();
        let script = match how {
            Bootstrap::Find => Some(find_script(token)),
            Bootstrap::Installed(id) => Some(installed_script(id, token)),
            Bootstrap::Direct => None,
        };
        if script.is_some() {
            cmd.args(["sh", "-s"]);
        } else {
            cmd.args(["a", "bridge", token]);
        }
        let prompt = self.prompt;
        let (child, stdin, stdout, stderr) =
            rt::try_unblock_with(&**rt, rt::BlockingClass::Storage, 64 << 10, move || {
                sys::spawn_ssh(&mut cmd, prompt)
            })
            .await??;
        let report = stderr.map(|stderr| Report::drain(rt, stderr)).transpose()?;
        let reactor = rt.reactor();
        let mut pipe = Pipe::new(
            rt::Async::new(&reactor, stdout)?,
            rt::Async::new(&reactor, stdin)?,
            Reaper {
                child: Some(child),
                rt: rt.clone(),
            },
        );
        let stream: &dyn Stream = &pipe;
        // A host that cannot run the script may hang up on it; its status says so.
        if let Some(script) = script {
            let _ = stream.write_all(script.as_bytes()).await;
        }
        let sync = format!("{SYNC} {token}");
        let ran = format!("{SHELL} {token}");
        let mut shell = false;
        let mut buffered = protocol::FrameReader::new();
        while let Some(line) = stream.line(&mut buffered).await? {
            shell |= line.trim_ascii_end() == ran.as_bytes();
            if line.trim_ascii_end().strip_prefix(sync.as_bytes()) == Some(WITH_ENV) {
                let Some(ClientMsg::Environment(env)) = stream.recv(&mut buffered).await? else {
                    return Err(
                        io::Error::other(format!("ssh {}: no environment", self.host)).into(),
                    );
                };
                return Ok(Bridge::Ready(Opened {
                    stream: Box::new(pipe),
                    machine: None,
                    buffered,
                    remote: Some(env),
                    // It may have just started the daemon there.
                    within: crate::START_TIMEOUT,
                }));
            }
        }
        let code = pipe.guard_mut().exited().await?;
        let said = match report {
            Some(report) => report.said().await,
            None => String::new(),
        };
        if code != Some(SSH_FAILED) {
            return Ok(Bridge::Ended(Ended { shell, code, said }));
        }
        Err(Error::Link(if needs_person(&said) {
            LinkError::NeedsInteractiveAuth(format!(
                "ssh {} needs a password or a host key confirmed, which this client cannot ask for; run `ssh {}` at a terminal once",
                self.host, self.host
            ))
        } else if said.is_empty() {
            LinkError::Unreachable(format!("ssh {} failed", self.host))
        } else {
            LinkError::Unreachable(format!("ssh {}: {said}", self.host))
        }))
    }

    /// Copies this build to the host layout's `runtime/<id>/a`; a host of
    /// another OS or architecture is refused. Blocking.
    fn install(&self, staged: &Staged) -> io::Result<()> {
        let quiet = || match self.prompt {
            Prompt::Terminal => Stdio::inherit(),
            Prompt::Never => Stdio::null(),
        };
        let uname = self
            .ssh()
            .args(["uname", "-sm"])
            .stdin(Stdio::null())
            .stderr(quiet())
            .output()?;
        let uname = String::from_utf8_lossy(&uname.stdout);
        let uname = uname.trim();
        let here = Platform::CURRENT;
        if Platform::from_uname(uname) != Some(here) {
            return Err(io::Error::other(format!(
                "`a` is not installed on {}, and this build is for {here}, not {uname:?}; install `a` there and retry",
                self.host
            )));
        }
        let (crc, size) = cksum(std::fs::File::open(&staged.exe)?)?;
        // The image's checksum goes as words; the script on stdin.
        let mut child = self
            .ssh()
            .args(["sh", "-s", &crc.to_string(), &size.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(quiet())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        stdin.write_all(install_script(&staged.id).as_bytes())?;
        stdin.flush()?;
        let mut said = String::new();
        stdout.read_line(&mut said)?;
        // The image follows only once the shell has read all of its
        // script: a shell may read ahead of the command it runs. A cut
        // upload shows in the status, which only a verified image passes.
        if said.trim_end() == "ready" {
            let _ = io::copy(&mut std::fs::File::open(&staged.exe)?, &mut stdin);
        }
        drop(stdin);
        let status = child.wait()?;
        match said.trim_end() {
            "installed" | "ready" if status.success() => Ok(()),
            "ready" => Err(io::Error::other(format!(
                "ssh {}: upload failed: {status}",
                self.host
            ))),
            _ => Err(io::Error::other(format!(
                "ssh {}: cannot prepare runtime directory: {status}",
                self.host
            ))),
        }
    }
}

/// Whether what ssh said is that a person must authenticate.
fn needs_person(said: &str) -> bool {
    said.contains("Permission denied") || said.contains("Host key verification failed")
}

/// One script for `sh -s`, read whole before any of it runs, so nothing
/// after it on stdin is taken for script or left for the bridge.
fn grouped(body: &str) -> String {
    format!("{{\n{body}\n}}\n")
}

pub(crate) fn find_script(token: &str) -> String {
    grouped(&format!(
        r#"echo {SHELL} {token}
if command -v a >/dev/null 2>&1; then exec a bridge {token}; fi
{}
if [ -x "$a_runtime/current/a" ]; then exec "$a_runtime/current/a" bridge {token}; else exit {NOT_FOUND}; fi"#,
        sys::RemoteLayout::discovery_script()
    ))
}

pub(crate) fn installed_script(id: &str, token: &str) -> String {
    grouped(&format!(
        "echo {SHELL} {token}\n{}\nexec \"$a_runtime/{id}/a\" bridge {token}",
        sys::RemoteLayout::for_platform(Platform::CURRENT).runtime_script()
    ))
}

/// Says `installed` when the image `$1 $2` (its `cksum`) is in place, else
/// `ready`, then takes the image on stdin. An image that does not match is
/// a crash leftover from an interrupted upload, never a build to run.
fn install_script(id: &str) -> String {
    // A cut connection ends `cat` with a clean EOF, so only the checksum
    // proves the upload whole. Staging is reconstructible; publication needs
    // checksum verification, not a machine-wide filesystem flush.
    grouped(&format!(
        r#"crc=$1 size=$2
verify() {{ [ -x "$1" ] && set -- $(cksum < "$1") && [ "$1 $2" = "$crc $size" ]; }}
{}
if verify "$a_runtime/{id}/a"; then echo installed; exit 0; fi
umask 077
mkdir -p "$a_runtime/{id}" && upload=$(mktemp -d "$a_runtime/{id}/.install.XXXXXXXXXX") || exit 1
trap 'rm -f "$upload/a"; rmdir "$upload"' 0
trap 'exit 1' HUP INT TERM
echo ready
cat > "$upload/a" && chmod 700 "$upload/a" && verify "$upload/a" || exit 1
ln "$upload/a" "$upload/../a" 2>/dev/null || verify "$upload/../a" || mv -f "$upload/a" "$upload/../a""#,
        sys::RemoteLayout::for_platform(Platform::CURRENT).runtime_script(),
    ))
}

/// POSIX `cksum` of a file: its CRC and its length.
fn cksum(mut file: impl Read) -> io::Result<(u32, u64)> {
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

/// Distinguishes our sync line from anything a login shell could print.
pub(crate) fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{:x}{:x}", nanos, std::process::id())
}

/// The end of what ssh says on stderr, read as it says it so that it never
/// blocks on a full pipe.
struct Report {
    tail: Arc<Mutex<Vec<u8>>>,
    done: rt::oneshot::Receiver<()>,
}

impl Report {
    const KEPT: usize = 4 << 10;

    fn drain(rt: &Rt, stderr: sys::ProcessPipe) -> io::Result<Self> {
        let pipe = rt::Async::read_pipe(&rt.reactor(), stderr)?;
        let tail = Arc::new(Mutex::new(Vec::new()));
        let (finished, done) = rt::oneshot::channel();
        let kept = tail.clone();
        rt.spawn(Box::pin(async move {
            let mut bytes = [0; 4096];
            while let Ok(read @ 1..) = pipe.read_with(|mut io| io.read(&mut bytes)).await {
                let mut tail = kept.lock().unwrap_or_else(PoisonError::into_inner);
                tail.extend_from_slice(&bytes[..read]);
                let over = tail.len().saturating_sub(Self::KEPT);
                tail.drain(..over);
            }
            finished.send(());
        }));
        Ok(Self { tail, done })
    }

    /// All it said, once ssh has exited.
    async fn said(self) -> String {
        self.done.await;
        let tail = self.tail.lock().unwrap_or_else(PoisonError::into_inner);
        crate::sanitize(String::from_utf8_lossy(&tail).trim()).into_owned()
    }
}

/// Kills and reaps the ssh child with its connection.
struct Reaper {
    child: Option<sys::ContainedChild>,
    rt: Rt,
}

impl Reaper {
    /// Its exit status, once it has exited.
    async fn exited(&mut self) -> io::Result<Option<i32>> {
        let child = self.child.as_mut().expect("bridge child");
        if child.try_wait()?.is_none()
            && let Ok(watch) = child.exit_watch()
        {
            rt::Async::for_reading(&self.rt.reactor(), watch)?
                .readable()
                .await;
        }
        Ok(child.try_wait()?.and_then(|status| status.code()))
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Revocation cannot wait behind network admission or process exit.
            let _ = child.kill();
            let rt = self.rt.clone();
            self.rt.spawn(Box::pin(async move {
                if let Ok(watch) = child
                    .exit_watch()
                    .and_then(|watch| rt::Async::for_reading(&rt.reactor(), watch))
                {
                    watch.readable().await;
                    let _ = child.wait();
                } else {
                    let _ = rt::try_unblock_with(&*rt, rt::BlockingClass::Storage, 0, move || {
                        child.wait()
                    })
                    .await;
                }
            }));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_verified_executable_uploads_are_published_without_global_sync() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sync = bin.join("sync");
        std::fs::write(&sync, "#!/bin/sh\necho global-sync >&2; exit 99\n").unwrap();
        std::fs::set_permissions(&sync, std::fs::Permissions::from_mode(0o700)).unwrap();
        let bytes = b"#!/bin/sh\nexit 0\n";
        let (crc, size) = cksum(&bytes[..]).unwrap();
        for (id, checksum) in [("valid", crc), ("invalid", crc ^ 1)] {
            let home = root.path().join(id);
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    &install_script(id),
                    "upload",
                    &checksum.to_string(),
                    &size.to_string(),
                ])
                .env("A_HOME", &home)
                .env(
                    "PATH",
                    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn().unwrap();
            child.stdin.take().unwrap().write_all(bytes).unwrap();
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.success(), id == "valid", "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let published = home.join("runtime").join(id).join("a");
            if id == "valid" {
                assert_eq!(std::fs::read(&published).unwrap(), bytes);
                assert_eq!(
                    std::fs::metadata(published).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            } else {
                assert!(!published.exists());
            }
        }
    }

    #[test]
    fn upload_checksums_match_the_hosts_cksum() {
        for len in [0, 1, 255, 256, 65_537, 200_003] {
            let bytes: Vec<u8> = (0..len).map(|n: u32| (n * 31 % 251) as u8).collect();
            let mut child = Command::new("cksum")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&bytes).unwrap();
            let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
            let (crc, size) = cksum(&bytes[..]).unwrap();
            assert_eq!(
                out.split_whitespace().collect::<Vec<_>>(),
                [crc.to_string(), size.to_string()]
            );
        }
    }

    /// dash reads its script from a pipe in blocks, so what follows the
    /// script on stdin is safe only once the script is read whole.
    #[test]
    fn a_shell_reading_ahead_leaves_the_bridge_nothing_of_the_script() {
        for shell in ["/bin/sh", "/bin/dash", "/bin/bash"] {
            if !std::path::Path::new(shell).exists() {
                continue;
            }
            let mut child = Command::new(shell)
                .arg("-s")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin
                .write_all(grouped("echo ready\nexec head -c 5").as_bytes())
                .unwrap();
            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            assert_eq!(line, "ready\n", "{shell}");
            stdin.write_all(b"frame").unwrap();
            drop(stdin);
            let mut rest = String::new();
            stdout.read_to_string(&mut rest).unwrap();
            assert_eq!(rest, "frame", "{shell}");
            child.wait().unwrap();
        }
    }
}
