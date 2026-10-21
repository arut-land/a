//! Daemon promises exercised through the shipped command interface on every host.

use std::fs;
use std::process::{Command, Output};

use sys::Home;

struct Isolated {
    home: Home,
}

impl Isolated {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("a-daemon-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let home = Home::from(root);
        fs::write(
            home.config_file(),
            "[daemon]\nagent = \"echo\"\nidle_secs = 60\n",
        )
        .unwrap();
        Self { home }
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_at(std::path::Path::new(env!("CARGO_BIN_EXE_a")), args)
    }

    fn command_at(&self, exe: &std::path::Path, args: &[&str]) -> Command {
        let mut command = Command::new(exe);
        command
            .args(args)
            .env("A_HOME", self.home.root())
            .env_remove("A_HOST")
            .current_dir(self.home.root());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn logs(&self) -> std::collections::BTreeMap<String, Vec<u8>> {
        fs::read_dir(self.home.sessions_dir())
            .unwrap()
            .map(|entry| entry.unwrap().path().join("log.jsonl"))
            .filter(|path| path.is_file())
            .map(|path| {
                (
                    path.parent()
                        .unwrap()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(&path).unwrap(),
                )
            })
            .collect()
    }

    fn pid(&self) -> u32 {
        fs::read_to_string(self.home.daemon_pid_file())
            .unwrap()
            .parse::<sys::crash::Record>()
            .unwrap()
            .pid
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        let _ = self.command(&["daemon", "stop"]).output();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match fs::remove_dir_all(self.home.root()) {
                Ok(()) => break,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => {
                    assert!(std::thread::panicking(), "cleaning isolated home: {error}");
                    break;
                }
            }
        }
    }
}

#[test]
fn concurrent_starters_share_one_daemon_and_stop_releases_its_lock() {
    let env = Isolated::new("concurrent");
    let clients: Vec<_> = (0..8)
        .map(|_| env.command(&["ls"]).spawn().unwrap())
        .collect();
    for child in clients {
        assert!(child.wait_with_output().unwrap().status.success());
    }
    let pid = env.pid();
    env.run(&["ls"]);
    assert_eq!(env.pid(), pid);
    env.run(&["daemon", "stop"]);
    assert!(client::wait_exited(
        &env.home,
        Some(std::time::Duration::from_secs(5))
    ));
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
    let output = env
        .command(&["--mode", "auto", "-p", "$ printf shell-ok > result.txt"])
        .current_dir(&dir)
        .output()
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
    assert!(client::wait_exited(
        &env.home,
        Some(std::time::Duration::from_secs(5))
    ));
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
        fs::metadata(source).unwrap().modified().unwrap() - std::time::Duration::from_secs(3600),
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
    let child = env
        .command(&["ls"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(10));
    if result.is_err() {
        env.run(&["daemon", "stop"]);
    }
    reader.join().unwrap();
    assert!(result.unwrap().unwrap().status.success());
    assert_ne!(std::process::id(), env.pid());
    assert!(!client::wait_exited(
        &env.home,
        Some(std::time::Duration::ZERO)
    ));
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
