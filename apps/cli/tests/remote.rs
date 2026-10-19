//! `a bridge` and `a --host`, with ssh and scp simulated by scripts that run
//! the "remote" command locally under a home of its own.
#![cfg(unix)]

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use protocol::{Body, ClientMsg, DaemonMsg, PROTO, Target, read_frame, write_frame};

const A: &str = env!("CARGO_BIN_EXE_a");

/// A scratch directory, removed (after stopping its daemons) on drop.
struct Scratch {
    root: PathBuf,
    homes: Vec<PathBuf>,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("a-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        common::warm_up(Path::new(A));
        Self {
            root: fs::canonicalize(root).unwrap(),
            homes: Vec::new(),
        }
    }

    /// An `A_HOME` whose daemon, running the echo agent, is stopped at
    /// the end.
    fn home(&mut self, path: PathBuf) -> PathBuf {
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("config.toml"), "[daemon]\nagent = \"echo\"\n").unwrap();
        self.homes.push(path.clone());
        path
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for home in &self.homes {
            let _ = Command::new(A)
                .args(["daemon", "stop"])
                .env("A_HOME", home)
                .output();
        }
        // A failed test's homes, with their daemons' logs, are kept.
        if std::thread::panicking() {
            eprintln!("kept {}", self.root.display());
        } else {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn a(home: &Path) -> Command {
    let mut cmd = Command::new(A);
    cmd.env("A_HOME", home).env_remove("A_HOST");
    cmd
}

#[test]
fn bridge_speaks_the_protocol_over_stdio() {
    let mut scratch = Scratch::new("bridge");
    let home = scratch.home(scratch.root.join("home"));
    let mut child = a(&home)
        .args(["bridge", "tok"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut sync = String::new();
    stdout.read_line(&mut sync).unwrap();
    assert_eq!(sync, "A-BRIDGE tok env\n");
    let Some(ClientMsg::Environment(shell)) = read_frame(&mut stdout).unwrap() else {
        panic!("no environment after the sync line");
    };
    assert_eq!(shell.cwd, std::env::current_dir().unwrap());

    let mut send = |msg: &ClientMsg| write_frame(&mut stdin, msg).unwrap();
    send(&ClientMsg::Hello {
        proto: PROTO,
        build: "far".into(),
        min: None,
        caps: protocol::Caps::NONE,
    });
    send(&ClientMsg::Attach {
        target: Target::New,
        workspace: "/w".into(),
        since: 0,
        env: None,
        decl: protocol::ClientDecl::default(),
    });
    send(&ClientMsg::input("hi"));
    let mut recv = || read_frame::<DaemonMsg>(&mut stdout).unwrap().unwrap();
    assert!(matches!(recv(), DaemonMsg::Welcome { proto: PROTO, .. }));
    assert!(matches!(recv(), DaemonMsg::Attached { .. }));
    assert!(matches!(recv(), DaemonMsg::Synced { seq: 0 }));
    assert!(matches!(
        recv(),
        DaemonMsg::Entry(e) if e.body == Body::User { text: "hi".into() }
    ));

    // Closing stdin ends the connection, and with it the bridge.
    drop(send);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

/// Fake `ssh` and `scp`: the remote is `$root/remote`, with its own `HOME`
/// and no `a` on its `PATH`, so the first connection installs this build.
fn fake_remote(scratch: &mut Scratch, uname: &str) -> (Command, PathBuf) {
    let remote = scratch.root.join("remote");
    scratch.home(remote.join(".a"));
    fs::create_dir_all(&remote).unwrap();
    let ssh = scratch.script(
        "ssh",
        &format!(
            r#"shift
echo "$*" >> '{log}'
[ "$*" = "uname -sm" ] && [ -n '{uname}' ] && {{ echo '{uname}'; exit; }}
cd '{remote}' && exec env -u A_HOME HOME='{remote}' PATH=/usr/bin:/bin sh -c "$*"
"#,
            log = scratch.root.join("ssh.log").display(),
            remote = remote.display(),
        ),
    );
    let scp = scratch.script(
        "scp",
        &format!(r#"cp "$2" '{}'/"${{3#*:}}""#, remote.display()),
    );
    let mut cmd = a(&scratch.home(scratch.root.join("local")));
    cmd.env("A_SSH", ssh)
        .env("A_SCP", scp)
        .env("PATH", "/usr/bin:/bin")
        .args(["--host", "box"]);
    (cmd, remote.join(".a"))
}

fn ok(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "a failed: {stderr}");
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn a_host_without_a_gets_this_build_installed() {
    let mut scratch = Scratch::new("install");
    let (mut new, remote) = fake_remote(&mut scratch, "");
    let session = ok(&new.arg("new").output().unwrap());

    let current = fs::read_link(remote.join("runtime/current")).unwrap();
    let installed = remote.join("runtime").join(&current).join("a");
    assert_eq!(fs::read(installed).unwrap(), fs::read(A).unwrap());
    assert!(remote.join("sessions").is_dir());
    assert!(!scratch.root.join("local/sessions").exists());

    // Now found through `runtime/current`; nothing is copied again.
    let (mut ls, _) = fake_remote(&mut scratch, "");
    let listed = ok(&ls.arg("ls").output().unwrap());
    assert!(listed.starts_with(session.trim()), "{listed}");
    let log = fs::read_to_string(scratch.root.join("ssh.log")).unwrap();
    assert_eq!(log.matches("uname -sm").count(), 1, "{log}");
}

#[test]
fn a_remote_session_runs_in_the_hosts_directory_and_environment() {
    let mut scratch = Scratch::new("remote-env");
    let (mut new, remote) = fake_remote(&mut scratch, "");
    let local = scratch.root.join("here");
    fs::create_dir_all(&local).unwrap();
    let session = ok(&new
        .arg("new")
        .current_dir(&local)
        .env("PATH", "/usr/bin:/bin:/only/here")
        .output()
        .unwrap());

    let dir = remote.join("sessions");
    let log = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    assert!(log.to_str().unwrap().contains(session.trim()), "{log:?}");
    let text = fs::read_to_string(log).unwrap();
    let mut lines = text.lines();
    let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let host = remote.parent().unwrap();
    assert_eq!(header["workspace"], host.to_str().unwrap());
    let entry: protocol::Entry = serde_json::from_str(lines.next().expect(&text)).unwrap();
    let Body::Environment(env) = entry.body else {
        panic!("{entry:?}")
    };
    assert_eq!(env.cwd, host);
    assert_eq!(env.vars["PATH"], "/usr/bin:/bin");
}

#[test]
fn a_host_of_another_platform_is_refused() {
    let mut scratch = Scratch::new("refuse");
    let (mut ls, remote) = fake_remote(&mut scratch, "Plan9 mips");
    let output = ls.arg("ls").output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("not installed on box"), "{stderr}");
    assert!(!remote.join("runtime").exists());
}

#[test]
fn a_newer_build_drains_the_old_daemon() {
    let mut scratch = Scratch::new("drain");
    let home = scratch.home(scratch.root.join("home"));
    ok(&a(&home).arg("new").output().unwrap());
    let old = fs::read_to_string(home.join("run/daemon.pid")).unwrap();

    // Another build of the same version, built later: trailing bytes change
    // the hash without changing what runs.
    let newer = scratch.root.join("newer-a");
    let mut bytes = fs::read(A).unwrap();
    bytes.extend(b"\0newer build");
    fs::write(&newer, bytes).unwrap();
    fs::set_permissions(&newer, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(&newer)
        .arg("ls")
        .env("A_HOME", &home)
        .output()
        .unwrap();
    assert_eq!(ok(&output).lines().count(), 1);

    let new = fs::read_to_string(home.join("run/daemon.pid")).unwrap();
    assert_ne!(old, new);
    let log = fs::read_to_string(home.join("logs/daemon.log")).unwrap();
    assert!(log.contains("drained for a newer build"), "{log}");
    let current = fs::read_link(home.join("runtime/current")).unwrap();
    assert_eq!(
        fs::read(home.join("runtime").join(current).join("a")).unwrap(),
        fs::read(&newer).unwrap()
    );
    // The old build stays staged, but no longer takes over.
    ok(&a(&home).arg("ls").output().unwrap());
    assert_eq!(
        fs::read_to_string(home.join("run/daemon.pid")).unwrap(),
        new
    );
}
