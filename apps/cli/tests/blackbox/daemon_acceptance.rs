//! Daemon promises exercised through the shipped command interface on every host.

use crate::support::{Isolated, TestProcess, deadline};
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn diagnosing_stopping_and_reading_logs_never_install_an_executable() {
    let env = Isolated::new("administration");
    for action in ["status", "log", "stop"] {
        env.run(&["daemon", action]);
        assert!(!env.home.runtime().dir().exists());
    }
}

#[test]
fn concurrent_starters_share_one_daemon_and_stop_releases_its_lock() {
    let env = Isolated::new("concurrent");
    let clients: Vec<_> = (0..8)
        .map(|_| TestProcess::spawn(&mut env.command(&["ls"])))
        .collect();
    for child in clients {
        assert!(child.collect(deadline()).unwrap().status.success());
    }
    let pid = env.pid();
    env.run(&["ls"]);
    assert_eq!(env.pid(), pid);
    env.run(&["daemon", "stop"]);
    assert!(client::wait_exited(&env.home, Some(Duration::from_secs(5))));
    env.run(&["ls"]);
    assert_ne!(env.pid(), pid);
}

#[test]
fn a_second_daemon_cannot_replace_or_unlink_the_live_endpoint() {
    let env = Isolated::new("endpoint-owner");
    env.run(&["ls"]);
    let pid = env.pid();
    let second = TestProcess::spawn(&mut env.command(&["__daemon"]))
        .collect(deadline())
        .unwrap();
    assert!(!second.status.success());
    env.home.endpoint().remove();
    env.run(&["ls"]);
    assert_eq!(env.pid(), pid);
}

#[test]
fn committed_turns_survive_a_daemon_restart() {
    let env = Isolated::new("durable");
    let output = env.run(&["-p", "durable hello"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("durable hello"));
    env.run(&["ls"]);
    let before = env.logs();
    env.run(&["daemon", "stop"]);
    env.run(&["ls"]);
    assert_eq!(before, env.logs());
    let output = env.run(&["-p", "after restart", "resume"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("after restart"));
}

#[test]
fn shell_paths_with_spaces_and_non_ascii_reach_the_native_directory() {
    let env = Isolated::new("shell");
    let dir = env.home.root().join("space café");
    fs::create_dir_all(&dir).unwrap();
    let mut command = env.command(&["--mode", "auto", "-p", "$ printf shell-ok > result.txt"]);
    command.current_dir(&dir);
    let output = TestProcess::spawn(&mut command)
        .collect(deadline())
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(dir.join("result.txt")).unwrap(), b"shell-ok");
}

#[test]
fn shell_created_temporary_files_stay_inside_the_isolated_home() {
    let env = Isolated::new("temporary space 'quote'");
    let script = if sys::find_bash(std::env::var("PATH").ok().as_deref()).is_some() {
        r#"p=$(mktemp "$TMPDIR/a-test.XXXXXXXX") && printf child-temp > "$p" && printf '%s' "$p" > temp-location"#
    } else {
        "$p=[IO.Path]::GetTempFileName(); [IO.File]::WriteAllText($p,'child-temp'); [IO.File]::WriteAllText((Join-Path $PWD 'temp-location'),$p)"
    };
    let prompt = format!("$ {script}");
    let mut command = env.command(&["--mode", "auto", "-p", &prompt]);
    let outside = env.home.root().join("outside");
    fs::create_dir(&outside).unwrap();
    command
        .env("TMPDIR", &outside)
        .env("TMP", &outside)
        .env("TEMP", &outside);
    let output = TestProcess::spawn(&mut command)
        .collect(deadline())
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let location = fs::read_to_string(env.home.root().join("temp-location")).unwrap();
    let location = sys::shell_directory(&location).canonicalize().unwrap();
    assert_eq!(
        location.parent().unwrap(),
        env.home.temporary_dir().canonicalize().unwrap()
    );
    assert_eq!(fs::read(location).unwrap(), b"child-temp");
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}

#[test]
fn an_unclean_exit_releases_the_endpoint_and_recovers_committed_sessions() {
    let env = Isolated::new("crash");
    env.run(&["-p", "before crash"]);
    env.run(&["ls"]);
    let before = env.logs();
    let pid = env.pid();
    crate::support::signal_process(pid, sys::Signal::Kill).unwrap();
    assert!(client::wait_exited(&env.home, Some(Duration::from_secs(5))));
    env.run(&["ls"]);
    assert_eq!(before, env.logs());
    assert_ne!(env.pid(), pid);
}

/// A build of `a` other than the one under test, and older: the same
/// program with a byte more, built an hour earlier.
fn older_build(env: &Isolated) -> std::path::PathBuf {
    use std::io::Write;
    let source = std::path::Path::new(env!("CARGO_BIN_EXE_a"));
    let older = env.home.root().join("older.exe");
    fs::copy(source, &older).unwrap();
    let mut file = fs::File::options().append(true).open(&older).unwrap();
    file.write_all(&[0]).unwrap();
    file.set_modified(
        fs::metadata(source).unwrap().modified().unwrap() - Duration::from_secs(3600),
    )
    .unwrap();
    older
}

#[test]
fn two_installs_upgrade_once_and_the_older_client_never_downgrades() {
    let env = Isolated::new("upgrade");
    let older = older_build(&env);
    assert!(env.command_at(&older, &["ls"]).status().unwrap().success());
    let old_pid = env.pid();
    let old_build = env.home.runtime().current().unwrap();
    env.run(&["ls"]);
    let new_pid = env.pid();
    assert_ne!(old_pid, new_pid);
    assert_ne!(env.home.runtime().current().unwrap(), old_build);
    assert!(env.command_at(&older, &["ls"]).status().unwrap().success());
    assert_eq!(env.pid(), new_pid);
}

/// The older build's daemon owns a job and runs a call gone on in the
/// background when the newer build's first client comes: both go to the
/// newer build live (in the same process on Unix, a successor on Windows,
/// a read in flight on the call's pipe cancelled and its bytes kept), which
/// reports the call's outcome and the job's end, their output whole.
#[test]
fn an_upgrade_hands_running_work_to_the_newer_build() {
    let env = Isolated::new("handover-job");
    let config = env.home.config_file();
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("\n[ext.shell]\ntimeout = 1\n");
    fs::write(&config, text).unwrap();
    let older = older_build(&env);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let gate = format!("exec 3<>/dev/tcp/127.0.0.1/{port}; printf ready >&3; read -r reply <&3");
    let job = format!("$ echo now; ({gate}; echo late) &");
    let ran = TestProcess::spawn(&mut env.command_at(&older, &["--mode", "auto", "-p", &job]))
        .collect(deadline())
        .unwrap();
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let mut job_release = child_at_gate(&listener);
    // Its client waits for the call's outcome, through the upgrade.
    let call = format!("$ echo start; {gate}; echo end; exit 7");
    let waiting = TestProcess::spawn(&mut env.command_at(&older, &["--mode", "auto", "-p", &call]));
    let mut call_release = child_at_gate(&listener);
    let detached = Instant::now() + Duration::from_secs(30);
    while !env
        .logs()
        .values()
        .any(|log| String::from_utf8_lossy(log).contains(r#""kind":"detached""#))
    {
        assert!(
            Instant::now() < detached,
            "the call never went on in the background"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let older_pid = env.pid();
    let older_build = env.home.runtime().current().unwrap();
    env.run(&["ls"]);
    let successor = env.home.runtime().current().unwrap();
    assert_ne!(successor, older_build);
    let upgraded = deadline();
    while !client::status(&env.home)
        .is_ok_and(|status| status.is_some_and(|status| status.build == successor))
    {
        assert!(
            Instant::now() < upgraded,
            "the successor never took ownership"
        );
        env.run(&["ls"]);
    }
    assert_eq!(
        env.pid() == older_pid,
        cfg!(unix),
        "where the newer build serves"
    );
    let logged = |text: &str| {
        env.logs()
            .values()
            .any(|log| String::from_utf8_lossy(log).contains(text))
    };
    assert!(!logged(r#""purpose":"job""#), "the gated job ended early");
    assert!(
        !logged(r#""purpose":"detached""#),
        "the gated call ended early"
    );
    job_release.write_all(b"done\n").unwrap();
    call_release.write_all(b"done\n").unwrap();
    let ended = Instant::now() + Duration::from_secs(30);
    while !(logged(r#""purpose":"job""#) && logged(r#""purpose":"detached""#)) {
        assert!(Instant::now() < ended, "the work's ends never came");
        std::thread::sleep(Duration::from_millis(100));
    }
    let waited = waiting.collect(deadline()).unwrap();
    let said = String::from_utf8_lossy(&waited.stdout);
    assert!(said.contains(r#""output":"start\nend\n""#), "{said}");
    assert!(said.contains(r#""exit":7"#), "{said}");
    let blobs = fs::read_dir(env.home.blobs_dir()).unwrap();
    let kept = (blobs.flatten())
        .filter(|fan| fan.file_name() != "open")
        .filter_map(|fan| fs::read_dir(fan.path()).ok())
        .flatten()
        .flatten()
        .any(|blob| fs::read(blob.path()).is_ok_and(|bytes| bytes == b"now\nlate\n"));
    assert!(kept, "the job's output was not kept");
    env.run(&["daemon", "stop"]);
    let committed = env.logs();
    env.run(&["ls"]);
    assert_eq!(
        env.logs(),
        committed,
        "the successor's outcomes were not durable"
    );
}

fn child_at_gate(listener: &TcpListener) -> std::net::TcpStream {
    let until = deadline();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Windows and macOS hand the listener's nonblocking mode to
                // accepted sockets, so the read below could see WouldBlock.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                let mut bytes = [0; 5];
                stream.read_exact(&mut bytes).unwrap();
                assert_eq!(&bytes, b"ready");
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "the child never reached its gate");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accepting a gated child: {error}"),
        }
    }
}

#[test]
fn a_cold_clients_output_reaches_eof_while_the_daemon_stays_alive() {
    let env = Isolated::new("stdio-eof");
    assert!(
        TestProcess::spawn(&mut env.command(&["ls"]))
            .collect(deadline())
            .unwrap()
            .status
            .success()
    );
    assert_ne!(std::process::id(), env.pid());
    assert!(!client::wait_exited(&env.home, Some(Duration::ZERO)));
}

#[test]
fn a_successor_starts_while_an_old_client_keeps_its_handle() {
    let env = Isolated::new("retained-client");
    env.run(&["ls"]);
    let previous = env.pid();
    let retained = env.home.endpoint().connect().unwrap();
    env.run(&["daemon", "stop"]);
    env.run(&["ls"]);
    assert_ne!(env.pid(), previous);
    drop(retained);
}

#[test]
fn output_larger_than_pipe_capacity_completes() {
    let env = Isolated::new("large-output");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "daemon_acceptance::process_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("A_TEST_PROCESS", "output")
        .env("A_HOME", env.home.root());
    let output = TestProcess::spawn(&mut command)
        .collect(deadline())
        .unwrap();
    assert!(output.status.success());
    assert!(
        output
            .stdout
            .windows(1_048_576)
            .any(|w| w.iter().all(|&b| b == b'o'))
    );
    assert_eq!(output.stderr, vec![b'e'; 1_048_576]);
}

#[test]
fn timeout_reaps_only_the_test_process_tree() {
    let env = Isolated::new("process-timeout");
    let command = || {
        let mut command = Command::new(sys::find_bash(None).unwrap());
        command
            .args(["-c", "sleep 600 & echo ready; wait"])
            .envs(std::env::vars_os())
            .env("A_HOME", env.home.root());
        command
    };
    let unrelated_output = env.home.root().join("unrelated");
    let unrelated =
        TestProcess::contained(&mut command(), fs::File::create(&unrelated_output).unwrap());
    let output = env.home.root().join("tree");
    let process = TestProcess::contained(&mut command(), fs::File::create(&output).unwrap());

    let ready_deadline = deadline();
    while !fs::read(&output).unwrap().starts_with(b"ready")
        || process.group().members().unwrap().len() < 2
        || !fs::read(&unrelated_output).unwrap().starts_with(b"ready")
    {
        assert!(Instant::now() < ready_deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let members = process.group().members().unwrap();
    let error = process
        .collect(Instant::now() + Duration::from_millis(20))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    while members
        .iter()
        .any(|&pid| sys::process_started(pid).is_some())
    {
        assert!(
            Instant::now() < ready_deadline,
            "timed out tree remains alive"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(unrelated.group().alive());
}

#[test]
fn every_case_uses_an_isolated_home() {
    let first = Isolated::new("same-case");
    let root;
    {
        let second = Isolated::new("same-case");
        root = second.home.root().to_owned();
        assert_ne!(first.home.root(), root);
        assert_eq!(
            second
                .command(&[])
                .get_envs()
                .find(|(key, _)| *key == "A_HOME")
                .unwrap()
                .1,
            Some(root.as_os_str())
        );
        first.run(&["new"]);
        assert_eq!(second.run(&["ls"]).stdout, b"");
        assert_ne!(first.pid(), second.pid());
        let stream: sys::OwnedConnection = second.home.endpoint().connect().unwrap();
        drop(stream);
    }
    assert!(!root.exists());
    assert!(first.home.root().exists());
}

#[test]
fn fixture_homes_use_the_runner_scratch_directory() {
    let scratch = crate::support::isolated_root("runner-scratch");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "daemon_acceptance::process_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("A_TEST_PROCESS", "root")
        .env("RUNNER_TEMP", &scratch)
        .env("A_HOME", &scratch);
    let result = TestProcess::spawn(&mut command).collect(deadline());
    crate::support::remove_home(&scratch);
    let output = result.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "subprocess fixture for process collection"]
fn process_fixture() {
    use std::io::Write;
    match std::env::var("A_TEST_PROCESS").as_deref() {
        Ok("output") => {
            io::stdout().write_all(&vec![b'o'; 1_048_576]).unwrap();
            io::stderr().write_all(&vec![b'e'; 1_048_576]).unwrap();
        }
        Ok("root") => {
            let root = crate::support::isolated_root("scratch-child");
            let parent = root.parent().unwrap().to_owned();
            crate::support::remove_home(&root);
            assert_eq!(
                parent,
                std::path::PathBuf::from(std::env::var_os("RUNNER_TEMP").unwrap())
            );
        }
        _ => {}
    }
}
