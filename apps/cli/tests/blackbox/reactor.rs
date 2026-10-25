//! What waiting costs the daemon: it waits on one reactor thread, so an
//! idle daemon never wakes, a command that runs quietly costs no thread,
//! and a background job's end is an event rather than something a poll
//! finds.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use crate::common::{
    Env, READ_TIMEOUT, thread_names, threads_and_switches, threads_parked, wait_for, wait_within,
};
use protocol::{Activity, Body, DaemonMsg, Target};

/// What `rt::Threads` names its blocking threads.
const BLOCKING: &str = "rt-blocking";

/// Waits until the daemon's blocking threads have retired: an idle one
/// goes after 10 s (`rt::Threads`), so a daemon with none left has settled
/// whatever its last turn started.
fn blocking_threads_retired(pid: u32) {
    wait_within(
        "the blocking threads to retire",
        Duration::from_secs(20),
        || !thread_names(pid).iter().any(|name| name == BLOCKING),
    );
}

/// An entry can precede the activity frame that certifies the turn ended.
fn wait_idle(conn: &mut crate::common::Conn) {
    while !matches!(
        conn.recv(),
        DaemonMsg::Activity {
            activity: Activity::Idle,
            ..
        }
    ) {}
}

fn parked(pid: u32) {
    wait_for("the daemon's threads to park", || threads_parked(pid));
}

#[test]
fn an_idle_daemon_never_wakes() {
    let env = Env::new("idle-wakes");
    let pid = env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    conn.input("hello");
    conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    wait_idle(&mut conn);
    drop(conn);
    blocking_threads_retired(pid);
    parked(pid);
    let (_, before) = threads_and_switches(pid);
    // The measurement: a window in which nothing happens to the daemon.
    std::thread::sleep(Duration::from_secs(3));
    let (threads, after) = threads_and_switches(pid);
    eprintln!(
        "idle daemon: {threads} threads, {} wakes in 3 s",
        after - before
    );
    assert_eq!(after - before, 0, "an idle daemon woke");
}

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
        wait_idle(conn);
    }
    // Less the blocking threads the log's writes woke, as many as happened
    // to overlap.
    let idle = thread_names(pid)
        .iter()
        .filter(|name| *name != BLOCKING)
        .count();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    for conn in &mut conns {
        conn.input(&format!(
            "$ exec 3<>/dev/tcp/127.0.0.1/{port}; printf ready >&3; read -r reply <&3"
        ));
    }
    for conn in &mut conns {
        conn.until(|e| matches!(e.body, Body::ToolProcess { .. }));
    }
    // Each shell has executed the command and stays alive until released;
    // neither machine load nor the measurement window can end it early.
    let mut releases = Vec::new();
    for _ in 0..CALLS {
        let mut accepted = None;
        wait_for("a quiet command to connect", || {
            accepted = listener.accept().ok();
            accepted.is_some()
        });
        let (mut stream, _) = accepted.unwrap();
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let mut ready = [0; 5];
        stream.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        releases.push(stream);
    }
    // A blocking thread a command held while it runs would never retire.
    blocking_threads_retired(pid);
    parked(pid);
    let (running, before) = threads_and_switches(pid);
    let running = usize::try_from(running).unwrap();
    // The measurement window.
    std::thread::sleep(Duration::from_secs(2));
    let (_, after) = threads_and_switches(pid);
    eprintln!(
        "threads: {idle} with {CALLS} sessions, {running} with {CALLS} gated commands running; \
         {} wakes in 2 s while they run",
        after - before
    );
    assert!(
        running <= idle,
        "{CALLS} quiet commands took {} threads",
        running - idle
    );
    assert_eq!(after - before, 0, "quiet commands woke the daemon");
    for stream in &mut releases {
        stream.write_all(b"done\n").unwrap();
    }
    for conn in &mut conns {
        conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
        wait_idle(conn);
    }
    env.cmd(&["daemon", "stop"]).status().unwrap();
}

/// A command past its timeout goes on in the background with no timer of
/// its own: while it runs quietly, the daemon never wakes.
#[test]
fn a_command_backgrounded_at_its_timeout_never_wakes_the_daemon() {
    let env = Env::new("timeout-idle");
    let pid = env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let command = format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port}; printf ready >&3; read -r reply <&3; echo released"
    );
    let input = serde_json::json!({ "command": command, "timeout": 1 });
    conn.input(&format!("call bash {input}"));
    conn.until(|e| matches!(e.body, Body::Detached { .. }));
    let mut accepted = None;
    wait_for("the command to connect", || {
        accepted = listener.accept().ok();
        accepted.is_some()
    });
    let (mut release, _) = accepted.unwrap();
    release.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut ready = [0; 5];
    release.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    blocking_threads_retired(pid);
    parked(pid);
    let (_, before) = threads_and_switches(pid);
    // The measurement window, the command running in the background.
    std::thread::sleep(Duration::from_secs(3));
    let (_, after) = threads_and_switches(pid);
    eprintln!(
        "{} wakes in 3 s while a backgrounded command runs",
        after - before
    );
    assert_eq!(after - before, 0, "a backgrounded command woke the daemon");
    release.write_all(b"done\n").unwrap();
    conn.until(
        |e| matches!(&e.body, Body::Assistant { text } if text.contains("ended in the background")),
    );
    env.cmd(&["daemon", "stop"]).status().unwrap();
}

#[test]
fn the_last_job_holds_the_daemon_until_it_ends() {
    let env = Env::new("job-exit");
    let config = env.home.config_file();
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("idle_secs = 15", "idle_secs = 1")).unwrap();
    let pid = env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    conn.input(&format!(
        "$ (exec 3<>/dev/tcp/127.0.0.1/{port}; printf ready >&3; read -r reply <&3) &"
    ));
    conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    wait_idle(&mut conn);
    let mut accepted = None;
    wait_for("the background job to connect", || {
        accepted = listener.accept().ok();
        accepted.is_some()
    });
    let (mut release, _) = accepted.unwrap();
    release.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut ready = [0; 5];
    release.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    drop(conn);
    // Retirement takes longer than the idle deadline. Only the job now
    // holds the daemon, and it cannot end before the test releases it.
    blocking_threads_retired(pid);
    parked(pid);
    release.write_all(b"done\n").unwrap();
    env.wait_exited("the daemon to exit after its last job");
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
    wait_idle(&mut first);
    parked(pid);
    let one = threads(pid);
    let conns: Vec<_> = (0..100)
        .map(|_| {
            let mut conn = env.connect();
            conn.attach(Target::Id(id), &env.ws);
            conn
        })
        .collect();
    parked(pid);
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
