//! What waiting costs the daemon: it waits on one reactor thread, so an
//! idle daemon never wakes, a command that runs quietly costs no thread,
//! and a background job's end is an event rather than something a poll
//! finds.
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::{Env, thread_names, threads_and_switches, wait_for, wait_within};
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

/// What `rt::Threads` names its blocking threads.
const BLOCKING: &str = "rt-blocking";

#[test]
fn a_running_quiet_command_costs_no_threads() {
    const CALLS: usize = 30;
    let env = Env::new("quiet-threads");
    let pid = env.start();
    let mut conns: Vec<_> = (0..CALLS)
        .map(|_| {
            let mut conn = env.connect();
            conn.attach(Target::New, &env.ws);
            conn
        })
        .collect();
    // Warm: one call each first, so what starts on first use has started.
    for conn in &mut conns {
        conn.input("$ true");
    }
    for conn in &mut conns {
        conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    }
    // Less the blocking threads the log's writes woke, as many as happened
    // to overlap.
    let idle = thread_names(pid)
        .iter()
        .filter(|name| *name != BLOCKING)
        .count();
    for conn in &mut conns {
        conn.input("$ sleep 30");
    }
    for conn in &mut conns {
        conn.until(|e| matches!(e.body, Body::ToolProcess { .. }));
    }
    // An idle blocking thread retires after 10 s; one that a command holds
    // while it runs never does.
    wait_within(
        "the blocking threads to retire",
        Duration::from_secs(20),
        || !thread_names(pid).iter().any(|name| name == BLOCKING),
    );
    let (running, before) = threads_and_switches(pid);
    let running = usize::try_from(running).unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let (_, after) = threads_and_switches(pid);
    eprintln!(
        "threads: {idle} with {CALLS} sessions, {running} with {CALLS} `sleep 30` running; \
         {} wakes in 2 s while they run",
        after - before
    );
    assert!(
        running <= idle,
        "{CALLS} quiet commands took {} threads",
        running - idle
    );
    assert_eq!(after - before, 0, "quiet commands woke the daemon");
    env.cmd(&["daemon", "stop"]).status().unwrap();
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

/// What `rt::Threads` names its async workers.
const WORKER: &str = "rt-worker";

/// The daemon's threads but its blocking pool's, which grows and retires
/// with blocking work rather than with clients.
fn threads(pid: u32) -> usize {
    thread_names(pid)
        .iter()
        .filter(|name| *name != BLOCKING)
        .count()
}

/// #33: a connection is two tasks on the reactor, not two threads: a
/// hundred clients attached cost the daemon no thread more than one.
#[test]
fn connections_add_no_threads() {
    let env = Env::new("conn-threads");
    let pid = env.start();
    let mut first = env.connect();
    let (id, _) = first.attach(Target::New, &env.ws);
    first.input("hello");
    first.until(|e| matches!(e.body, Body::Assistant { .. }));
    let one = threads(pid);
    let conns: Vec<_> = (0..100)
        .map(|_| {
            let mut conn = env.connect();
            conn.attach(Target::Id(id), &env.ws);
            conn
        })
        .collect();
    let hundred = threads(pid);
    eprintln!("daemon threads: {one} with 1 client, {hundred} with 101");
    assert!(
        thread_names(pid)
            .iter()
            .any(|name| name.starts_with(WORKER)),
        "{:?}",
        thread_names(pid)
    );
    assert_eq!(hundred, one, "{:?}", thread_names(pid));
    drop(conns);
}

/// #58: nothing a connection does holds up the others: with two hundred
/// clients connected and a hundred of them attached but never reading
/// while a session floods them, a fresh client is served at once.
#[test]
fn nothing_blocks_the_reactor() {
    let env = Env::new("no-block");
    env.start();
    let mut busy = env.connect();
    let (id, _) = busy.attach(Target::New, &env.ws);
    // Never read: their sockets fill and their writers wait.
    let stuck: Vec<_> = (0..100)
        .map(|_| {
            let mut conn = env.connect();
            conn.attach(Target::Id(id), &env.ws);
            conn
        })
        .collect();
    // Connected, saying nothing more.
    let idle: Vec<_> = (0..100).map(|_| env.connect()).collect();
    for n in 0..20 {
        busy.input(&format!(
            "$ head -c 200000 /dev/zero | tr '\\0' x; echo {n}"
        ));
    }
    let started = Instant::now();
    let mut fresh = env.connect();
    fresh.attach(Target::New, &env.ws);
    fresh.input("still here");
    fresh.until(|e| matches!(e.body, Body::Assistant { .. }));
    let took = started.elapsed();
    eprintln!("a fresh client's turn among 200 others: {took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");
    drop((stuck, idle));
}
