//! M1 acceptance: the engine's promises, checked against the built binary.
#![cfg(unix)]

mod common;

use std::fs;
use std::process::Command;

use common::{Env, assert_contiguous, group_alive, is_tool_call, result_of, wait_for};
use protocol::{Body, Outcome, Target};
use serde_json::json;

const ANY: Target = Target::Latest {
    any_workspace: true,
};

#[test]
fn concurrent_cold_starts_make_exactly_one_daemon() {
    let env = Env::new("one-daemon");
    let clients: Vec<_> = (0..8).map(|_| env.cmd(&["ls"]).spawn().unwrap()).collect();
    for mut client in clients {
        assert!(client.wait().unwrap().success());
    }
    assert_eq!(env.daemons(), [env.pid().unwrap()]);
}

/// The cold client binds the socket itself and connects into the new
/// daemon's backlog, so it never sleeps waiting for the daemon to come up.
/// Only the client is traced: strace would otherwise wait out the daemon.
#[cfg(target_os = "linux")]
#[test]
fn a_cold_client_never_polls_for_the_daemon() {
    let env = Env::new("no-polling");
    let trace = env.root.join("trace");
    let wrapper = [
        "strace",
        "-e",
        "trace=connect,nanosleep,clock_nanosleep",
        "-o",
        trace.to_str().unwrap(),
    ];
    let status = env.cmd_under(&wrapper, &["ls"]).status();
    assert!(status.expect("this test needs strace").success());
    let trace = fs::read_to_string(trace).unwrap();
    assert!(trace.lines().any(|l| l.starts_with("connect(")), "{trace}");
    assert!(!trace.contains("sleep("), "{trace}");
}

#[test]
fn resume_after_the_terminal_closes_mid_turn_replays_every_entry() {
    let env = Env::new("resume");
    let mut term = env.repl(&[]);
    term.lines_until("[session");
    term.type_line("hello");
    term.type_line("sleep 1");
    term.lines_until("tool sleep");
    let hangup = Command::new("kill")
        .args(["-HUP", &term.child.id().to_string()])
        .status();
    assert!(hangup.unwrap().success());
    term.child.wait().unwrap();

    let mut conn = env.connect();
    let (id, mut entries) = conn.attach(ANY, &env.ws);
    let call = entries.iter().find(|e| is_tool_call(e)).unwrap().seq;
    if !entries.iter().any(|e| result_of(e, call).is_some()) {
        entries.extend(conn.until(|e| result_of(e, call).is_some()));
    }
    assert_contiguous(&entries);
    assert_eq!(entries, env.log(id));
    let bodies: Vec<_> = entries.into_iter().map(|e| e.body).collect();
    assert_eq!(
        bodies,
        [
            Body::User {
                text: "hello".into()
            },
            Body::Assistant {
                text: "hello".into()
            },
            Body::User {
                text: "sleep 1".into()
            },
            Body::ToolCall {
                tool: "sleep".into(),
                input: json!({ "seconds": 1 }),
                id: None,
            },
            Body::ToolResult {
                call,
                outcome: Outcome::Done {
                    output: json!({ "slept": 1 })
                }
            },
        ]
    );

    let mut resumed = env.repl(&["resume"]);
    let mut shown = resumed.lines_until("└");
    shown.remove(0);
    assert_eq!(
        shown,
        [
            "you  hello",
            "a    hello",
            "you  sleep 1",
            r#"tool sleep {"seconds":1}"#,
            r#"  └ {"slept":1}"#,
        ]
    );
    resumed.finish();
}

#[test]
fn two_terminals_on_one_session_see_identical_state() {
    let env = Env::new("two-terminals");
    env.start();
    let mut first = env.connect();
    let (id, _) = first.attach(Target::New, &env.ws);
    let mut second = env.connect();
    second.attach(Target::Id(id), &env.ws);

    for text in ["one", "$ echo two", "sleep 0"] {
        first.input(text);
    }
    let done = |e: &protocol::Entry| match &e.body {
        Body::ToolResult {
            outcome: Outcome::Done { output },
            ..
        } => output["slept"] == 0,
        _ => false,
    };
    let seen = [first.until(done), second.until(done)];
    let (_, replayed) = env.connect().attach(Target::Id(id), &env.ws);

    assert_contiguous(&seen[0]);
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], replayed);
    assert_eq!(seen[0], env.log(id));
}

#[test]
fn a_safe_tool_reruns_after_the_daemon_is_killed() {
    let env = Env::new("kill-safe");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    conn.input("sleep 2");
    let call = conn.until(is_tool_call).pop().unwrap().seq;
    env.kill_daemon();
    let torn = env.log(id);

    env.start();
    let mut conn = env.connect();
    let (_, mut entries) = conn.attach(Target::Id(id), &env.ws);
    if !entries.iter().any(|e| result_of(e, call).is_some()) {
        entries.extend(conn.until(|e| result_of(e, call).is_some()));
    }

    assert_eq!(torn.len(), 2);
    assert_contiguous(&entries);
    assert_eq!(entries, env.log(id));
    let results: Vec<_> = entries.iter().filter_map(|e| result_of(e, call)).collect();
    assert_eq!(
        results,
        [&Outcome::Done {
            output: json!({ "slept": 2 })
        }]
    );
}

#[test]
fn an_unsafe_tool_is_interrupted_and_its_processes_killed_after_the_daemon_is_killed() {
    let env = Env::new("kill-unsafe");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    conn.input("$ echo ran >> ran.txt; sleep 30");
    let entries = conn.until(|e| matches!(e.body, Body::ToolProcess { .. }));
    let Body::ToolProcess { call, pgid, .. } = entries.last().unwrap().body else {
        unreachable!()
    };
    let ran = env.ws.join("ran.txt");
    wait_for("the command to start", || ran.exists());
    env.kill_daemon();
    let orphaned = group_alive(pgid);

    env.start();
    let mut conn = env.connect();
    let (_, mut entries) = conn.attach(Target::Id(id), &env.ws);
    if !entries.iter().any(|e| result_of(e, call).is_some()) {
        entries.extend(conn.until(|e| result_of(e, call).is_some()));
    }
    wait_for("the leftover process group to die", || !group_alive(pgid));

    assert!(orphaned, "the tool's processes died with the daemon");
    assert_contiguous(&entries);
    assert_eq!(entries, env.log(id));
    let results: Vec<_> = entries.iter().filter_map(|e| result_of(e, call)).collect();
    assert_eq!(results, [&Outcome::Interrupted]);
    assert_eq!(fs::read_to_string(ran).unwrap(), "ran\n");
}

#[test]
#[ignore = "store::load skips a torn tail but appends after it, so the next entry fuses with it and is lost"]
fn entries_after_a_torn_tail_survive_a_restart() {
    let env = Env::new("torn-tail");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    conn.input("one");
    conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    drop(conn);
    env.run(&["daemon", "stop"]);
    let log = env.home.sessions_dir().join(format!("{id}.jsonl"));
    let mut text = fs::read_to_string(&log).unwrap();
    text.push_str(r#"{"seq":3,"ep"#);
    fs::write(&log, text).unwrap();

    env.start();
    let mut conn = env.connect();
    conn.attach(Target::Id(id), &env.ws);
    conn.input("two");
    conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    drop(conn);
    env.run(&["daemon", "stop"]);

    env.start();
    let (_, entries) = env.connect().attach(Target::Id(id), &env.ws);
    assert_eq!(entries.len(), 4);
    assert_contiguous(&entries);
    assert_eq!(entries, env.log(id));
}

#[test]
fn sessions_outlive_their_daemon() {
    let env = Env::new("outlive");
    let id = env.run(&["new"]).trim().to_owned();
    assert_eq!(env.run(&["daemon", "stop"]), "stopped\n");
    let listed = env.run(&["ls"]);
    assert!(listed.starts_with(&id), "{listed}");
}
