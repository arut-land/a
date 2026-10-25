//! `a bridge` and `a --host`, with ssh simulated by scripts that run
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
    assert_eq!(shell.cwd.local(), env.ws);

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
        DaemonMsg::Entry(e) if matches!(&e.body, Body::Input { segments: text, taken: true, .. } if text == "hi")
    ));

    // Closing stdin ends the connection, and with it the bridge.
    drop(send);
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

/// Fake `ssh`: the remote has a literal path containing shell metacharacters,
/// its own `HOME`
/// and no `a` on its `PATH`, so the first connection installs this build.
/// Its login shell, like csh or fish, runs only plain words: anything to
/// quote is refused. Returns `a --host box` for the test's own (local)
/// home, and the host's.
fn fake_remote(env: &mut Env, uname: &str) -> (Command, Home) {
    let remote = env.root.join("remote space 'quote' $(touch injected)");
    let host = env.other_home(remote.join(".a"));
    let quote =
        |path: &std::path::Path| format!("'{}'", path.to_str().unwrap().replace('\'', r"'\''"));
    let ssh = env.script(
        "ssh",
        &format!(
            r#"shift
printf '%s\n' "$*" >> {log}
case "$*" in *[!A-Za-z0-9\ ._+-]*) echo "the login shell cannot parse: $*" >&2; exit 1;; esac
[ "$*" = "uname -sm" ] && [ -n '{uname}' ] && {{ echo '{uname}'; exit; }}
cd {remote} || exit
# A connection cut mid-upload: the remote sees a clean EOF. `dd bs=1`
# passes the script on as it comes, where `head -c` would wait to fill.
case "${{A_TEST_CUT:-}} $*" in [0-9]*" sh -s "[0-9]*)
    dd bs=1 count="$A_TEST_CUT" 2>/dev/null | env A_HOME={home} HOME={remote} PATH=/usr/bin:/bin sh -c "$*"; exit;;
esac
exec env A_HOME={home} HOME={remote} PATH=/usr/bin:/bin sh -c "$*"
"#,
            log = quote(&env.root.join("ssh.log")),
            remote = quote(&remote),
            home = quote(host.root()),
        ),
    );
    let mut cmd = env.cmd(&["--host", "box"]);
    cmd.env("A_SSH", ssh).env("PATH", "/usr/bin:/bin");
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
fn a_host_reuses_its_installed_identity_when_discovery_is_lost() {
    let mut env = Env::new("install");
    let (mut new, host) = fake_remote(&mut env, "");
    let session = ok(&new.arg("new").output().unwrap());

    let current = fs::read_link(host.root().join("runtime/current")).unwrap();
    let installed = host.root().join("runtime").join(&current).join("a");
    assert_eq!(fs::read(&installed).unwrap(), fs::read(&env.exe).unwrap());
    assert!(host.sessions_dir().is_dir());
    assert!(!env.home.sessions_dir().exists(), "a session at home");

    // Now found through `runtime/current`; nothing is copied again.
    let (mut ls, _) = fake_remote(&mut env, "");
    let listed = ok(&ls.arg("ls").output().unwrap());
    assert!(listed.starts_with(session.trim()), "{listed}");
    let log = fs::read_to_string(env.root.join("ssh.log")).unwrap();
    assert_eq!(log.matches("uname -sm").count(), 1, "{log}");
    assert_eq!(installs(&log), 1, "{log}");
    // The remote root's command substitution stayed literal.
    let remote = host.root().parent().unwrap();
    assert!(!remote.join("injected").exists());
    assert!(!env.root.join("injected").exists());

    let modified = fs::metadata(&installed).unwrap().modified().unwrap();
    fs::remove_file(host.runtime().dir().join("current")).unwrap();
    let (mut ls, _) = fake_remote(&mut env, "");
    assert!(ok(&ls.arg("ls").output().unwrap()).starts_with(session.trim()));
    // Asked again, the host finds the image in place and takes no upload.
    let log = fs::read_to_string(env.root.join("ssh.log")).unwrap();
    assert_eq!(installs(&log), 2, "{log}");
    assert_eq!(
        fs::metadata(installed).unwrap().modified().unwrap(),
        modified
    );
}

/// The install sessions in an ssh log: `sh -s <crc> <size>`.
fn installs(log: &str) -> usize {
    log.lines()
        .filter(|command| {
            command.strip_prefix("sh -s ").is_some_and(|rest| {
                rest.split(' ').count() == 2
                    && rest
                        .split(' ')
                        .all(|word| word.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .count()
}

/// Fake `ssh` to a host whose `a` on PATH is `a`, run by `login` (a shell
/// command line in `$cmd`); returns `a --host box` and the host's home.
fn host_with(env: &mut Env, a: &std::path::Path, login: &str) -> (Command, Home) {
    let remote = env.root.join("remote");
    let bin = remote.join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(a, bin.join("a")).unwrap();
    let host = env.other_home(remote.join(".a"));
    let ssh = env.script(
        "ssh",
        &format!(
            r#"shift
cmd="$*"
printf '%s\n' "$cmd" >> '{log}'
cd '{remote}' || exit
export A_HOME='{home}' HOME='{remote}' PATH='{bin}:/usr/bin:/bin'
{login}
"#,
            log = env.root.join("ssh.log").display(),
            remote = remote.display(),
            home = host.root().display(),
            bin = bin.display(),
        ),
    );
    let mut cmd = env.cmd(&["--host", "box"]);
    cmd.env("A_SSH", ssh).env("PATH", "/usr/bin:/bin");
    (cmd, host)
}

#[test]
fn a_host_without_sh_is_served_by_the_a_its_login_shell_finds() {
    let mut env = Env::new("remote-no-sh");
    let exe = env.exe.clone();
    // As cmd or PowerShell: no `sh`, but `a` on PATH runs.
    let (mut ls, _) = host_with(
        &mut env,
        &exe,
        r#"case "$cmd" in "sh -s"*) echo "'sh' is not recognized as an internal or external command" >&2; exit 1;; esac
exec sh -c "$cmd""#,
    );
    ok(&ls.arg("ls").output().unwrap());
    let log = fs::read_to_string(env.root.join("ssh.log")).unwrap();
    let commands: Vec<_> = log.lines().collect();
    assert!(
        matches!(commands[..], ["sh -s", direct] if direct.starts_with("a bridge ")),
        "{log}"
    );
}

#[test]
fn an_a_that_sh_found_and_that_failed_is_reported_not_retried() {
    let mut env = Env::new("remote-a-fails");
    let broken = env.script("broken", "echo 'a: no daemon here' >&2\nexit 3\n");
    let (mut ls, host) = host_with(&mut env, &broken, r#"exec sh -c "$cmd""#);
    let output = ls.arg("ls").output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("a: no daemon here"), "{stderr}");
    assert!(stderr.contains("status 3"), "{stderr}");
    let log = fs::read_to_string(env.root.join("ssh.log")).unwrap();
    assert_eq!(log.lines().collect::<Vec<_>>(), ["sh -s"], "{log}");
    assert!(!host.root().join("runtime").exists());
}

#[test]
fn the_remote_login_shell_is_only_given_plain_words() {
    let mut env = Env::new("plain-words");
    let (mut new, _) = fake_remote(&mut env, "");
    ok(&new.arg("new").output().unwrap());
    let log = fs::read_to_string(env.root.join("ssh.log")).unwrap();
    for command in log.lines() {
        assert!(
            command
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b" ._+-".contains(&b)),
            "{command:?}"
        );
    }
    // Found, installed and run: each through `sh -s` or plain words.
    assert!(log.contains("uname -sm"), "{log}");
    assert!(log.lines().any(|c| c == "sh -s"), "{log}");
}

#[test]
fn a_cut_remote_upload_is_never_published_or_reused() {
    let mut env = Env::new("install-cut");
    let (mut cut, host) = fake_remote(&mut env, "");
    let output = cut.env("A_TEST_CUT", "4096").arg("ls").output().unwrap();
    assert!(!output.status.success(), "{output:?}");
    let runtime = host.root().join("runtime");
    let published = fs::read_dir(&runtime)
        .unwrap()
        .filter(|id| id.as_ref().unwrap().path().join("a").exists())
        .count();
    assert_eq!(published, 0, "a partial image was published");

    let (mut ls, _) = fake_remote(&mut env, "");
    ok(&ls.arg("ls").output().unwrap());
    let current = fs::read_link(runtime.join("current")).unwrap();
    let installed = runtime.join(&current).join("a");
    assert_eq!(fs::read(&installed).unwrap(), fs::read(&env.exe).unwrap());

    // A partial image left by an older installer is replaced, not trusted.
    let partial = runtime.join(&current).join("partial");
    fs::write(&partial, &fs::read(&env.exe).unwrap()[..4096]).unwrap();
    fs::set_permissions(
        &partial,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    fs::rename(&partial, &installed).unwrap();
    fs::remove_file(runtime.join("current")).unwrap();
    let (mut ls, _) = fake_remote(&mut env, "");
    ok(&ls.arg("ls").output().unwrap());
    assert_eq!(fs::read(&installed).unwrap(), fs::read(&env.exe).unwrap());
}

#[test]
fn native_remote_discovery_works_across_platforms_without_an_isolated_home() {
    for uname in ["Linux x86_64", "Darwin arm64"] {
        let env = Env::new("remote-native-layout");
        let home = env.root.join("remote home");
        let data = env.root.join("remote data");
        let platform = sys::Platform::from_uname(uname).unwrap();
        let runtime = Command::new("sh")
            .args([
                "-c",
                &format!(
                    "{}\nprintf '%s' \"$a_runtime\"",
                    sys::RemoteLayout::for_platform(platform).runtime_script()
                ),
            ])
            .env_remove("A_HOME")
            .env("HOME", &home)
            .env("XDG_DATA_HOME", &data)
            .output()
            .unwrap();
        assert!(runtime.status.success());
        let runtime = std::path::PathBuf::from(String::from_utf8(runtime.stdout).unwrap());
        let current = runtime.join("current");
        fs::create_dir_all(&current).unwrap();
        let marker = env.root.join("discovered");
        let found = env.script(
            "found",
            &format!("printf '%s' \"$1\" > '{}'\nexit 7", marker.display()),
        );
        fs::rename(found, current.join("a")).unwrap();
        let os = uname.split_whitespace().next().unwrap();
        env.script("uname", &format!("printf '%s\\n' '{os}'"));
        let ssh = env.script(
            "ssh",
            &format!(
                "shift\nexec env -u A_HOME HOME='{home}' XDG_DATA_HOME='{data}' PATH='{bin}:/usr/bin:/bin' sh -c \"$*\"",
                home = home.display(),
                data = data.display(),
                bin = env.root.display(),
            ),
        );
        let output = env
            .cmd(&["--host", "box", "ls"])
            .env("A_SSH", ssh)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            fs::read_to_string(&marker).ok().as_deref(),
            Some("bridge"),
            "{uname}: {output:?}"
        );
    }
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
    assert_eq!(shell.cwd.local(), remote);
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
    assert_eq!(shell.cwd.local(), host.root().parent().unwrap());
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
