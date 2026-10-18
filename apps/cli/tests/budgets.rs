//! Latency and memory budgets. `mise run budgets` runs them on the release
//! build: memory is held to its budget there, latency too except on CI.
#![cfg(unix)]

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{Env, latency_budget, release};

#[test]
fn a_cold_start_hears_from_the_daemon_within_50ms() {
    let mut times: Vec<Duration> = (0..5)
        .map(|run| {
            let env = Env::new(&format!("cold-{run}"));
            // A build is staged, and macOS scans the staged copy, once per
            // build: run its daemon once, then measure a start with none.
            assert!(env.cmd(&["ls"]).status().unwrap().success());
            assert!(env.cmd(&["daemon", "stop"]).status().unwrap().success());
            let start = Instant::now();
            let mut term = env.repl(&[]);
            let first = term.line();
            let elapsed = start.elapsed();
            assert!(first.starts_with("[session"), "{first}");
            term.finish();
            elapsed
        })
        .collect();
    times.sort();
    let median = times[times.len() / 2];
    let limit = latency_budget(Duration::from_millis(50));
    eprintln!("cold start to first daemon reply: {times:?}");
    assert!(
        median < limit,
        "median {median:?} over {limit:?}: {times:?}"
    );
}

#[test]
fn an_idle_daemon_stays_under_10mb() {
    let env = Env::new("idle-rss");
    let pid = env.start();
    std::thread::sleep(Duration::from_secs(5));
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let kib: u32 = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let limit = if release() { 10 * 1024 } else { 20 * 1024 };
    eprintln!("idle daemon RSS: {kib} KiB");
    assert!(kib < limit, "{kib} KiB over {limit} KiB");
}

#[test]
fn the_terminal_client_paints_its_first_frame_within_16ms() {
    let mut times: Vec<Duration> = (0..5)
        .map(|run| {
            let env = Env::new(&format!("tui-cold-{run}"));
            assert!(env.cmd(&["ls"]).status().unwrap().success());
            assert!(env.cmd(&["daemon", "stop"]).status().unwrap().success());
            let term = env.tty(&[], 100, 30);
            term.wait_for("● live");
            term.first_frame().expect("no frame")
        })
        .collect();
    times.sort();
    let median = times[times.len() / 2];
    let limit = latency_budget(Duration::from_millis(16));
    eprintln!("terminal client, spawn to first frame: {times:?}");
    assert!(
        median < limit,
        "median {median:?} over {limit:?}: {times:?}"
    );
}

#[test]
fn an_idle_terminal_client_does_not_run() {
    let env = Env::new("tui-idle");
    let term = env.tty(&[], 100, 30);
    term.wait_for("● live");
    std::thread::sleep(Duration::from_millis(500));
    let pid = term.child.id();
    let before = cpu_time(pid);
    std::thread::sleep(Duration::from_secs(3));
    let used = cpu_time(pid).saturating_sub(before);
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let rss = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    eprintln!("idle terminal client: {used:?} of CPU in 3 s, {rss} KiB RSS");
    assert!(
        used <= Duration::from_millis(10),
        "{used:?} of CPU while idle"
    );
}

/// A process's user and system time so far.
fn cpu_time(pid: u32) -> Duration {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // utime and stime, in clock ticks (100 a second), after the
        // parenthesized command name.
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let ticks = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
        return Duration::from_millis(ticks * 10);
    }
    // `ps` elsewhere: M:SS.cc.
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let (min, sec) = text.trim().split_once(':').unwrap();
    Duration::from_secs_f64(min.parse::<f64>().unwrap() * 60.0 + sec.parse::<f64>().unwrap())
}
