//! Daemon promises exercised through the shipped command interface on every host.

use crate::support::{Isolated, TestProcess, deadline};
use std::fs;
use std::io;
use std::process::Command;
use std::time::{Duration, Instant};

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
fn an_unclean_exit_releases_the_endpoint_and_recovers_committed_sessions() {
    let env = Isolated::new("crash");
    env.run(&["-p", "before crash"]);
    env.run(&["ls"]);
    let before = env.logs();
    let pid = env.pid();
    sys::signal_process(pid, sys::Signal::Kill).unwrap();
    assert!(client::wait_exited(&env.home, Some(Duration::from_secs(5))));
    env.run(&["ls"]);
    assert_eq!(before, env.logs());
    assert_ne!(env.pid(), pid);
}

#[test]
fn two_installs_upgrade_once_and_the_older_client_never_downgrades() {
    use std::io::Write;
    let env = Isolated::new("upgrade");
    let source = std::path::Path::new(env!("CARGO_BIN_EXE_a"));
    let older = env.home.root().join("older.exe");
    fs::copy(source, &older).unwrap();
    let mut file = fs::File::options().append(true).open(&older).unwrap();
    file.write_all(&[0]).unwrap();
    file.set_modified(
        fs::metadata(source).unwrap().modified().unwrap() - Duration::from_secs(3600),
    )
    .unwrap();
    drop(file);
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
        || process.group().members().len() < 2
        || !fs::read(&unrelated_output).unwrap().starts_with(b"ready")
    {
        assert!(Instant::now() < ready_deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let members = process.group().members();
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
        let stream: sys::Stream = second.home.endpoint().connect().unwrap();
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
