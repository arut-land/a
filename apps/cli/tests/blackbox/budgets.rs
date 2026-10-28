//! Latency and memory budgets: statements about the release build on a
//! machine doing nothing else, so `mise run budgets` is what runs them
//! (`A_BIN`, one at a time) and the dev-build suite skips them. Memory is
//! held to its budget there, latency too except on CI lanes that leave
//! `A_STRICT_BUDGETS` unset. A latency is a wall time across two
//! processes (the client's wait on the daemon is the thing measured), so
//! nothing but a quiet machine makes its tail mean anything: under a
//! parallel suite the p99 doubles while the median does not move.

use std::process::Command;
use std::time::{Duration, Instant};

use crate::common::{Env, latency_budget, release};
use protocol::{Body, Target};

/// This test's home, on the release build these budgets are stated for.
fn env(name: &str) -> Env {
    assert!(
        release(),
        "budgets are measured on the release build: mise run budgets"
    );
    Env::new(name)
}

/// The sorted times' value at percentile `p`.
fn percentile(sorted: &[Duration], p: usize) -> Duration {
    sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]
}

/// What launching any process costs on this machine: the median of a few
/// runs of `/usr/bin/true`, below which no cold start can go.
fn floor() -> Duration {
    let mut times: Vec<Duration> = (0..5)
        .map(|_| {
            let start = Instant::now();
            assert!(Command::new("/usr/bin/true").status().unwrap().success());
            start.elapsed()
        })
        .collect();
    times.sort();
    percentile(&times, 50)
}

#[test]
#[ignore = "mise run budgets"]
fn a_cold_start_hears_from_the_daemon_within_50ms_over_the_floor() {
    let floor = floor();
    let mut times: Vec<Duration> = (0..5)
        .map(|run| {
            let env = env(&format!("cold-{run}"));
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
    let median = percentile(&times, 50);
    let limit = latency_budget(Duration::from_millis(50));
    eprintln!(
        "cold start to first daemon reply: median {median:?}, min {:?}, max {:?}; \
         process floor {floor:?}",
        times[0],
        times[times.len() - 1]
    );
    assert!(
        median.saturating_sub(floor) < limit,
        "median {median:?} over {limit:?} + floor {floor:?}: {times:?}"
    );
}

/// A client attaching to a warm daemon's session with some history:
/// connect, hello, attach and the replay up to `Synced`.
#[test]
#[ignore = "mise run budgets"]
fn attaching_takes_under_2ms_at_p99() {
    let env = env("attach");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    for n in 0..10 {
        conn.input(&format!("message {n}"));
        conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    }
    drop(conn);
    // Twenty entries, and a `queued` one for each message sent before the
    // turn ahead of it had ended.
    let logged = env.log(id).len();
    let mut times: Vec<Duration> = (0..500)
        .map(|_| {
            let start = Instant::now();
            let (_, replay) = env.connect().attach(Target::Id(id), &env.ws);
            let elapsed = start.elapsed();
            assert_eq!(replay.len(), logged);
            elapsed
        })
        .collect();
    times.sort();
    let (p50, p95, p99) = (
        percentile(&times, 50),
        percentile(&times, 95),
        percentile(&times, 99),
    );
    let limit = latency_budget(Duration::from_millis(2));
    eprintln!(
        "attach with a {logged}-entry replay: min {:?}, p50 {p50:?}, p95 {p95:?}, p99 {p99:?}",
        times[0]
    );
    assert!(p99 < limit, "p99 {p99:?} over {limit:?}");
}

#[test]
#[ignore = "mise run budgets"]
fn the_terminal_client_paints_its_first_frame_within_16ms() {
    let mut times: Vec<Duration> = (0..5)
        .map(|run| {
            let env = env(&format!("tui-cold-{run}"));
            assert!(env.cmd(&["ls"]).status().unwrap().success());
            assert!(env.cmd(&["daemon", "stop"]).status().unwrap().success());
            let term = env.tty(&[], 100, 30);
            term.wait_for("● live");
            term.first_frame().expect("no frame")
        })
        .collect();
    times.sort();
    let median = percentile(&times, 50);
    let limit = latency_budget(Duration::from_millis(16));
    eprintln!("terminal client, spawn to first frame: {times:?}");
    assert!(
        median < limit,
        "median {median:?} over {limit:?}: {times:?}"
    );
}

/// Idle, the daemon's charged memory stays under 10 MiB and its terminal client runs no
/// code: both measured over the one quiet window.
#[test]
#[ignore = "mise run budgets"]
fn an_idle_daemon_and_terminal_client_cost_nothing() {
    let env = env("idle");
    let term = env.tty(&[], 100, 30);
    term.wait_for("● live");
    let (daemon, client) = (env.pid().unwrap(), term.child.id());
    let before = cpu_time(client);
    // The measurement: a window in which nothing happens to either.
    std::thread::sleep(Duration::from_secs(3));
    let used = cpu_time(client).saturating_sub(before);
    let (daemon_rss, client_rss) = (rss_kib(daemon), rss_kib(client));
    let charged = sys::process_memory::charged_bytes(daemon).unwrap();
    eprintln!(
        "idle: daemon {} KiB {} ({daemon_rss} KiB RSS); terminal client {used:?} of CPU in 3 s, \
         {client_rss} KiB RSS",
        charged / 1024,
        sys::process_memory::METRIC,
    );
    assert!(
        charged < 10 * 1024 * 1024,
        "daemon {} bytes {} over 10 MiB",
        charged,
        sys::process_memory::METRIC,
    );
    assert!(
        used <= Duration::from_millis(10),
        "{used:?} of CPU while idle"
    );
}

fn rss_kib(pid: u32) -> u32 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
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
