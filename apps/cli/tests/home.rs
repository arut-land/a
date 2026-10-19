//! The daemon owns its home: its environment, its config file, its lock
//! and crash breaker, and the session list, whichever client started it.
#![cfg(unix)]

mod common;

use std::fs;
use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::process::Stdio;

use common::Env;
use protocol::{Body, Caps, ClientMsg, DaemonMsg, Outcome, PROTO, Target, read_frame, write_frame};

/// The client that starts the daemon does not lend it its environment: a
/// key in it never serves another client's session.
#[test]
fn a_clients_key_never_serves_another_clients_session() {
    let env = Env::new("own-env");
    // The model-backed agent.
    fs::write(env.home.config_file(), "[daemon]\nidle_secs = 15\n").unwrap();
    let first = env
        .cmd(&["new"])
        .env("OPENAI_API_KEY", "sk-first-client")
        // Nothing answers here: a call that took the first client's
        // variables would fail to connect rather than for want of a key.
        .env("OPENAI_BASE_URL", "http://127.0.0.1:9/v1")
        .output()
        .unwrap();
    assert!(first.status.success(), "{first:?}");

    // Another client's session, with no environment of its own.
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    conn.input("hi");
    let entries = conn.until(|e| matches!(e.body, Body::ModelResult { .. }));
    let Body::ModelResult {
        outcome: Outcome::Failed { error },
        ..
    } = &entries.last().unwrap().body
    else {
        panic!("the model call did not fail: {entries:?}");
    };
    assert!(error.contains("OPENAI_API_KEY"), "{error}");
}

/// After a crash loop a daemon refuses to serve, and says why: the daemon
/// holding the lock is the only one that counts crashes and checks the
/// breaker, however it was started.
#[test]
fn a_crash_loop_is_the_daemons_to_refuse() {
    let env = Env::new("breaker");
    fs::create_dir_all(env.home.run_dir()).unwrap();
    fs::write(env.home.run_dir().join("crashes"), "3\n").unwrap();

    // Started by hand, without a client's lock: it refuses all the same.
    let mut by_hand = env.cmd(&["__daemon"]).spawn().unwrap();
    // Not by connecting: the first connection it answers is its last.
    common::wait_for("the socket", || env.home.endpoint().path().exists());
    let mut conn = UnixStream::connect(env.home.endpoint().path()).unwrap();
    write_frame(
        &mut conn,
        &ClientMsg::Hello {
            proto: PROTO,
            build: "acceptance".into(),
            min: None,
            caps: Caps::NONE,
            client: None,
        },
    )
    .unwrap();
    let answer = read_frame::<DaemonMsg>(&mut BufReader::new(&conn)).unwrap();
    assert!(
        matches!(&answer, Some(DaemonMsg::Error { message }) if message.contains("crashed 3 times")),
        "{answer:?}"
    );
    drop(conn);
    assert!(!by_hand.wait().unwrap().success());

    // Started by a client, which tells the user what the daemon said.
    let out = env.cmd(&["ls"]).output().unwrap();
    let told = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        told.contains("crashed 3 times") && told.contains("`a daemon restart`"),
        "{told}"
    );
    common::wait_for("the refusing daemon to go", || env.daemons().is_empty());

    env.run(&["daemon", "restart"]);
    env.run(&["ls"]);
    let count = fs::read_to_string(env.home.run_dir().join("crashes")).ok();
    assert!(count.as_ref().is_none_or(|n| n.trim() == "0"), "{count:?}");
}

/// What the daemon can answer, it is asked: the session a prefix names,
/// its status, and to stop.
#[test]
fn the_daemon_resolves_prefixes_reports_its_status_and_stops_when_asked() {
    let env = Env::new("ask");
    let short = env.run(&["new"]).trim().to_owned();

    let mut repl = env.repl(&["attach", &short[..5]]);
    let shown = repl.lines_until("[session");
    assert!(shown.last().unwrap().contains(&short), "{shown:?}");
    repl.finish();
    // Its stdin held open: it ends on the daemon's answer, not on EOF.
    let mut missing = env
        .cmd(&["attach", "zzzz"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = missing.stdin.take();
    let mut told = String::new();
    missing
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut told)
        .unwrap();
    assert!(!missing.wait().unwrap().success());
    drop(stdin);
    assert!(told.contains("no session matches zzzz"), "{told}");

    let status = env.run(&["daemon", "status"]);
    let pid = env.pid().unwrap();
    assert!(
        status.contains(&format!("running (pid {pid}")) && status.contains("1 sessions"),
        "{status}"
    );
    assert_eq!(env.run(&["daemon", "stop"]).trim(), "stopped");
    assert_eq!(env.pid(), None);
    let log = fs::read_to_string(env.home.daemon_log()).unwrap();
    assert!(log.contains("asked to stop, exiting"), "{log}");
}
