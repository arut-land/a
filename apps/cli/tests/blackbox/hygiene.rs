//! No test leaves a process behind, however it ends: what it started is
//! in its process group, which its watchdog kills once the test is gone
//! (see `common::Group`). Each end is a test of its own, run in a process
//! of its own: on the dev build a panic runs none of the test's
//! destructors (cranelift unwinds through its frames without their
//! cleanups), so no guard in the test could be what ends them.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use crate::common::pty::Pty;
use crate::common::{Env, Repl, group_alive, processes_under, wait_for};

/// What a test may have running when it ends.
struct Started {
    _term: Pty,
    _repl: Repl,
    /// Dropped without a kill, as `std` drops a child.
    _web: Child,
    _forks: Child,
}

/// A terminal client, a line client, `a web`, the daemon they share, and
/// a script with a child of its own, which the test never sees.
fn start(env: &Env) -> Started {
    let term = env.tty(&[], 80, 24);
    term.wait_for("● live");
    let mut repl = env.repl(&[]);
    repl.lines_until("[session");
    let mut web = env
        .cmd(&["web", "--no-open"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut link = String::new();
    BufReader::new(web.stdout.take().unwrap())
        .read_line(&mut link)
        .unwrap();
    assert!(link.starts_with("a web: http://"), "{link:?}");
    let forks = env.script("forks", "sleep 600 & echo started; wait");
    let mut forks = env
        .cmd_under(&[forks.to_str().unwrap()], &[])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut started = String::new();
    BufReader::new(forks.stdout.take().unwrap())
        .read_line(&mut started)
        .unwrap();
    assert_eq!(started, "started\n");
    assert!(env.pid().is_some(), "no daemon");
    Started {
        _term: term,
        _repl: repl,
        _web: web,
        _forks: forks,
    }
}

/// Runs [`ends`] in a process of its own, ending as `how` says, and checks
/// that nothing it started outlives it.
fn assert_ends_clean(how: &str) {
    let mut test = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "hygiene::ends", "--ignored", "--nocapture"])
        .env("A_TEST_ENDS", how)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let said = BufReader::new(test.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix("started ").map(str::to_owned))
        .expect("the test said nothing");
    let (group, root) = said.split_once(' ').unwrap();
    let (root, group) = (PathBuf::from(root), group.parse().unwrap());
    if how == "kill" {
        test.kill().unwrap();
    }
    assert!(!test.wait().unwrap().success());
    wait_for("what the test started to end", || {
        !group_alive(group) && processes_under(&root).is_empty()
    });
    // Kept, as a failed test's home is.
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_panicking_test_leaves_no_process_behind() {
    assert_ends_clean("panic");
}

/// Killed outright (a runner's timeout, or the runner itself killed), a
/// test runs no code at all at its end.
#[test]
fn a_killed_test_leaves_no_process_behind() {
    assert_ends_clean("kill");
}

/// Run by [`assert_ends_clean`]: starts everything, says so, and ends as
/// `A_TEST_ENDS` says (`panic`, or `kill` to wait to be killed).
#[test]
#[ignore = "the hygiene tests run it"]
fn ends() {
    let Some(how) = std::env::var_os("A_TEST_ENDS") else {
        return;
    };
    let env = Env::new("hygiene");
    let _started = start(&env);
    println!("started {} {}", env.group(), env.root.display());
    assert!(how != "panic", "a test failing with everything running");
    loop {
        std::thread::park();
    }
}
