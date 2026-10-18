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
