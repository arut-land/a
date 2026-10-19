//! What waiting costs the daemon: it waits on one reactor thread, so an
//! idle daemon never wakes, a command that runs quietly costs no thread,
//! and a background job's end is an event rather than something a poll
//! finds.
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::{Env, threads_and_switches, wait_for};
use protocol::{Body, Target};

#[test]
fn an_idle_daemon_never_wakes() {
    let env = Env::new("idle-wakes");
    let pid = env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    conn.input("hello");
    conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    drop(conn);
    // What the turn started settles, and blocking threads it woke retire
    // (rt::Threads lets one go after 10 s idle).
    std::thread::sleep(Duration::from_secs(11));
    let (_, before) = threads_and_switches(pid);
    std::thread::sleep(Duration::from_secs(3));
    let (threads, after) = threads_and_switches(pid);
    eprintln!(
        "idle daemon: {threads} threads, {} wakes in 3 s",
        after - before
    );
    assert_eq!(after - before, 0, "an idle daemon woke");
}

#[test]
fn a_job_end_is_seen_at_once() {
    let env = Env::new("job-exit");
    let config = env.home.config_file();
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("idle_secs = 15", "idle_secs = 1")).unwrap();
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    conn.input("$ sleep 3 >/dev/null &");
    conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    drop(conn);
    let started = Instant::now();
    // The job holds the daemon up; once it ends, the daemon idles out
    // within its second.
    wait_for("the daemon to exit", || env.pid().is_none());
    let took = started.elapsed();
    eprintln!("daemon exit after a 3 s job: {took:?}");
    // 3 s of job and 1 s of idleness; a poll would add its period.
    assert!(took >= Duration::from_millis(3500), "{took:?}");
    assert!(took < Duration::from_millis(4600), "{took:?}");
}
