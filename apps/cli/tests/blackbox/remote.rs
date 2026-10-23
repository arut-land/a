//! `a bridge` and `a --host`, with ssh and scp simulated by scripts that run
//! the "remote" command locally under a home of its own.

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};

use crate::common::{Env, hello};
use protocol::{Body, ClientMsg, DaemonMsg, PROTO, Target, read_frame, write_frame};
use sys::Home;

#[test]
fn bridge_speaks_the_protocol_over_stdio() {
    let env = Env::new("bridge");
    let mut child = env
        .cmd(&["bridge", "tok"])
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
    assert_eq!(shell.cwd, env.ws);

    let mut send = |msg: &ClientMsg| write_frame(&mut stdin, msg).unwrap();
    send(&hello());
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
    assert!(matches!(recv(), DaemonMsg::Entry(e) if matches!(e.body, Body::Extensions { .. })));
    assert!(matches!(
        recv(),
        DaemonMsg::Activity {
            seq: 1,
            activity: protocol::Activity::Idle
        }
    ));
    assert!(matches!(recv(), DaemonMsg::Synced { seq: 1 }));
    assert!(matches!(
        recv(),
        DaemonMsg::Entry(e) if matches!(&e.body, Body::Input { text, taken: true, .. } if text == "hi")
    ));

    // Closing stdin ends the connection, and with it the bridge.
    drop(send);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

/// Fake `ssh` and `scp`: the remote is `<root>/remote`, with its own `HOME`
/// and no `a` on its `PATH`, so the first connection installs this build.
/// Returns `a --host box` for the test's own (local) home, and the host's.
fn fake_remote(env: &mut Env, uname: &str) -> (Command, Home) {
    let remote = env.root.join("remote");
    let host = env.other_home(remote.join(".a"));
    let ssh = env.script(
        "ssh",
        &format!(
            r#"shift
echo "$*" >> '{log}'
[ "$*" = "uname -sm" ] && [ -n '{uname}' ] && {{ echo '{uname}'; exit; }}
cd '{remote}' && exec env A_HOME='{remote}/.a' HOME='{remote}' PATH=/usr/bin:/bin sh -c "$*"
"#,
            log = env.root.join("ssh.log").display(),
            remote = remote.display(),
        ),
    );
    let scp = env.script("scp", r#"cp "$2" "${3#*:}""#);
    let mut cmd = env.cmd(&["--host", "box"]);
    cmd.env("A_SSH", ssh)
        .env("A_SCP", scp)
        .env("PATH", "/usr/bin:/bin");
    (cmd, host)
}

fn ok(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "a failed: {stderr}");
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// The one session log in `home`, beside what is derived from it, as
/// text.
fn only_log(home: &Home) -> String {
    let mut logs = fs::read_dir(home.sessions_dir())
        .unwrap()
        .map(|item| item.unwrap().path().join("log.jsonl"))
        .filter(|path| path.is_file());
    let log = logs.next().unwrap();
    assert!(logs.next().is_none());
    fs::read_to_string(log).unwrap()
}

/// The session's workspace from its header, and the environment it runs
/// in (logged after the extensions it runs with).
fn workspace_and_env(text: &str) -> (String, protocol::Environment) {
    let mut lines = text.lines();
    let header: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let entry: protocol::Entry = serde_json::from_str(lines.nth(1).expect(text)).unwrap();
    let Body::Environment(env) = entry.body else {
        panic!("{entry:?}")
    };
    (header["workspace"].as_str().unwrap().to_owned(), env)
}

#[test]
fn a_host_without_a_gets_this_build_installed() {
    let mut env = Env::new("install");
    let (mut new, host) = fake_remote(&mut env, "");
    let session = ok(&new.arg("new").output().unwrap());

    let current = fs::read_link(host.root().join("runtime/current")).unwrap();
    let installed = host.root().join("runtime").join(&current).join("a");
    assert_eq!(fs::read(installed).unwrap(), fs::read(&env.exe).unwrap());
    assert!(host.sessions_dir().is_dir());
    assert!(!env.home.sessions_dir().exists(), "a session at home");

    // Now found through `runtime/current`; nothing is copied again.
    let (mut ls, _) = fake_remote(&mut env, "");
    let listed = ok(&ls.arg("ls").output().unwrap());
    assert!(listed.starts_with(session.trim()), "{listed}");
    let log = fs::read_to_string(env.root.join("ssh.log")).unwrap();
    assert_eq!(log.matches("uname -sm").count(), 1, "{log}");
}

#[test]
fn a_remote_session_runs_in_the_hosts_directory_and_environment() {
    let mut env = Env::new("remote-env");
    let (mut new, host) = fake_remote(&mut env, "");
    let local = env.root.join("here");
    fs::create_dir_all(&local).unwrap();
    let session = ok(&new
        .arg("new")
        .current_dir(&local)
        .env("PATH", "/usr/bin:/bin:/only/here")
        .output()
        .unwrap());

    let text = only_log(&host);
    assert!(text.contains(session.trim()), "{text}");
    let (workspace, shell) = workspace_and_env(&text);
    let remote = host.root().parent().unwrap();
    assert_eq!(workspace, remote.to_str().unwrap());
    assert_eq!(shell.cwd, remote);
    assert_eq!(shell.vars["PATH"], "/usr/bin:/bin");
}

/// The daemon finds a remote session's workspace from where it started,
/// with the host's git variables, as it does for a local one.
#[test]
fn a_remote_session_started_in_a_subdirectory_works_in_its_git_root() {
    let mut env = Env::new("remote-git");
    // The host's home, where ssh lands, is in a checkout.
    fs::create_dir_all(env.root.join(".git")).unwrap();
    let (mut new, host) = fake_remote(&mut env, "");
    ok(&new.arg("new").output().unwrap());

    let (workspace, shell) = workspace_and_env(&only_log(&host));
    assert_eq!(workspace, env.root.to_str().unwrap());
    assert_eq!(shell.cwd, host.root().parent().unwrap());
}

#[test]
fn a_host_of_another_platform_is_refused() {
    let mut env = Env::new("refuse");
    let (mut ls, host) = fake_remote(&mut env, "Plan9 mips");
    let output = ls.arg("ls").output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("not installed on box"), "{stderr}");
    assert!(!host.root().join("runtime").exists());
}
