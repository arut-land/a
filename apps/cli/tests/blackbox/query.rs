//! Queries through the client SDK against the built daemon: the fixtures'
//! `echo` query, about a workspace.

use std::path::Path;
use std::time::Instant;

use client::{Answer, Connection, Daemon};
use protocol::{ClientMsg, DaemonMsg, Json, Outcome, Scope, ScopeKind};
use serde_json::json;

use crate::support::{Isolated, deadline};

/// A daemon with one session, which `a` started in the home's root, and
/// the session's workspace.
fn started(name: &str) -> (Isolated, Daemon, client::Answers, std::path::PathBuf) {
    let env = Isolated::new(name);
    env.run(&["-p", "hello"]);
    let staged = (env.home.runtime())
        .stage(Path::new(env!("CARGO_BIN_EXE_a")))
        .unwrap();
    let daemon = Daemon::local(env.home.clone(), staged);
    let session = daemon.sessions().unwrap().remove(0);
    // Held open, so the daemon is busy with one query throughout.
    let held = daemon
        .query("echo", here(&env), hold("held"))
        .expect("asked");
    (env, daemon, held, session.workspace)
}

/// The home's root, where the session started: a directory names its
/// workspace as an attach from it finds it.
fn here(env: &Isolated) -> Scope {
    Scope::Workspace(env.home.root().to_path_buf())
}

fn hold(text: &str) -> Json {
    Json::encode(&json!({ "text": text, "times": 1, "hold": true })).unwrap()
}

fn echo(daemon: &Daemon, scope: Scope, body: &serde_json::Value) -> Vec<Answer> {
    let body = Json::encode(body).unwrap();
    let answers = daemon.query("echo", scope, body).unwrap();
    answers.map(Result::unwrap).collect()
}

fn update(text: &str) -> Answer {
    Answer::Update(Json::encode(text).unwrap())
}

fn echoed(text: &str, workspace: &Path) -> Answer {
    let output = Json::encode(&json!({ "text": text, "workspace": workspace })).unwrap();
    Answer::Reply(Outcome::Done { output })
}

#[test]
fn a_daemon_advertises_the_queries_it_answers() {
    let (_env, daemon, _held, _) = started("query-welcome");
    let welcome = daemon.connect(&|| {}).unwrap();
    assert!(
        (welcome.queries.iter()).any(|q| q.name == "echo" && q.scope == ScopeKind::Workspace),
        "{:?}",
        welcome.queries
    );
}

#[test]
fn a_streamed_answer_is_its_updates_then_one_reply_and_no_log_changes() {
    let (env, daemon, _held, workspace) = started("query-stream");
    let before = env.logs();
    let streamed = echo(&daemon, here(&env), &json!({ "text": "hi", "times": 2 }));
    assert_eq!(
        streamed,
        [update("hi"), update("hi"), echoed("hi", &workspace)]
    );
    assert_eq!(env.logs(), before, "a query changed a log");
}

#[test]
fn a_query_answers_about_the_scope_it_is_asked_or_one_implying_it() {
    let (_env, daemon, _held, workspace) = started("query-scope");
    let session = daemon.sessions().unwrap().remove(0).id;
    assert_eq!(
        echo(&daemon, Scope::Session(session), &json!({ "text": "s" })),
        [echoed("s", &workspace)]
    );
    let [Answer::Reply(Outcome::Failed { error })] =
        &echo(&daemon, Scope::Daemon, &json!({ "text": "d" }))[..]
    else {
        panic!("a workspace query answered about the daemon");
    };
    assert_eq!(error, "echo answers about a workspace");
}

#[test]
fn a_burst_larger_than_a_connection_holds_arrives_whole() {
    let (env, daemon, _held, workspace) = started("query-burst");
    // Far more than the outbox's 256 frames: the daemon paces the stream
    // to the reader instead of closing the connection.
    let times = 200_000;
    let mut answers = daemon
        .query(
            "echo",
            here(&env),
            Json::encode(&json!({ "text": "b", "times": times })).unwrap(),
        )
        .unwrap();
    let updates = answers
        .by_ref()
        .take(times)
        .filter(|answer| *answer.as_ref().unwrap() == update("b"))
        .count();
    assert_eq!(updates, times);
    assert_eq!(answers.next().unwrap().unwrap(), echoed("b", &workspace));
    assert!(answers.next().is_none());
}

#[test]
fn a_cancelled_query_replies_interrupted_and_nothing_follows() {
    let (_env, _daemon, mut held, _) = started("query-cancel");
    assert_eq!(held.next().unwrap().unwrap(), update("held"));
    held.canceller().cancel().unwrap();
    assert_eq!(
        held.next().unwrap().unwrap(),
        Answer::Reply(Outcome::Interrupted { output: None })
    );
    assert!(held.next().is_none());
}

/// The most queries the daemon runs at once, the one [`started`] holds
/// among them: its outbox budget's 256 requests, less the 16 it reserves
/// for control.
const RUNNING: u64 = 256 - 16;

/// Holds `n` queries open on one connection, each seen running.
fn hold_open(env: &Isolated, daemon: &Daemon, n: u64) -> Connection {
    let mut conn = daemon.connect(&|| {}).unwrap();
    for id in 1..=n {
        let query = ClientMsg::Query {
            id,
            service: "echo".into(),
            scope: here(env),
            body: hold("open"),
        };
        conn.writer.send(&query).unwrap();
    }
    for _ in 0..n {
        let msg = conn.reader.recv().unwrap().unwrap();
        assert!(matches!(msg, DaemonMsg::Update { .. }), "{msg:?}");
    }
    conn
}

#[test]
fn open_queries_never_block_a_sessions_input() {
    let (env, daemon, _held, _) = started("query-input");
    let _open = hold_open(&env, &daemon, RUNNING - 1);
    let [Answer::Reply(Outcome::Failed { error })] =
        &echo(&daemon, here(&env), &json!({ "text": "x" }))[..]
    else {
        panic!("a query was admitted past the budget");
    };
    assert_eq!(error, "daemon admission is full");
    let said = env.run(&["-p", "still heard", "resume"]);
    assert!(String::from_utf8_lossy(&said.stdout).contains("still heard"));
}

#[test]
fn hanging_up_cancels_a_query_and_frees_its_admission() {
    let (env, daemon, _held, _) = started("query-hangup");
    let open = hold_open(&env, &daemon, RUNNING - 1);
    let full = echo(&daemon, here(&env), &json!({ "text": "x" }));
    assert!(
        matches!(&full[..], [Answer::Reply(Outcome::Failed { .. })]),
        "{full:?}"
    );
    drop(open);
    let until = deadline();
    loop {
        match &echo(&daemon, here(&env), &json!({ "text": "x" }))[..] {
            [Answer::Reply(Outcome::Done { .. })] => break,
            [Answer::Reply(Outcome::Failed { error })] if error == "daemon admission is full" => {
                assert!(Instant::now() < until, "admission was never freed");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "dev builds' cranelift backend cannot unwind through these frames: \
              CARGO_PROFILE_DEV_CODEGEN_BACKEND=llvm cargo nextest run -p a --test blackbox \
              query:: --run-ignored all"
)]
fn a_query_that_panics_fails_and_the_daemon_goes_on() {
    let (env, daemon, _held, workspace) = started("query-panic");
    let [Answer::Reply(Outcome::Failed { error })] =
        &echo(&daemon, here(&env), &json!({ "text": "p", "panic": true }))[..]
    else {
        panic!("a panicking query did not fail");
    };
    assert!(error.contains("asked to"), "{error}");
    assert_eq!(
        echo(&daemon, here(&env), &json!({ "text": "after" })),
        [echoed("after", &workspace)]
    );
}
