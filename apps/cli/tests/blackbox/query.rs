//! Queries through the client SDK against the built daemon: the fixtures'
//! `echo` query, about a workspace.

use std::path::Path;

use client::{Answer, Daemon};
use protocol::{Json, Outcome, Scope, ScopeKind};
use serde_json::json;

use crate::support::Isolated;

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
fn a_query_is_answered_by_id_and_never_logged() {
    let env = Isolated::new("query");
    env.run(&["-p", "hello"]);
    let staged = (env.home.runtime())
        .stage(Path::new(env!("CARGO_BIN_EXE_a")))
        .unwrap();
    let daemon = Daemon::local(env.home.clone(), staged);
    let session = daemon.sessions().unwrap().remove(0);
    let before = env.logs();

    let welcome = daemon.connect(&|| {}).unwrap();
    assert!(
        (welcome.queries.iter()).any(|q| q.name == "echo" && q.scope == ScopeKind::Workspace),
        "{:?}",
        welcome.queries
    );
    drop(welcome);

    // A directory names its workspace as an attach from it finds it: the
    // session's, which `a` started there.
    let dir = Scope::Workspace(env.home.root().to_path_buf());
    let streamed = echo(&daemon, dir.clone(), &json!({ "text": "hi", "times": 2 }));
    let workspace = &session.workspace;
    assert_eq!(
        streamed,
        [update("hi"), update("hi"), echoed("hi", workspace)]
    );
    // A workspace query about a session answers about its workspace.
    assert_eq!(
        echo(&daemon, Scope::Session(session.id), &json!({ "text": "s" })),
        [echoed("s", workspace)]
    );
    let [Answer::Reply(Outcome::Failed { error })] =
        &echo(&daemon, Scope::Daemon, &json!({ "text": "d" }))[..]
    else {
        panic!("a workspace query answered about the daemon");
    };
    assert_eq!(error, "echo answers about a workspace");

    let held = Json::encode(&json!({ "text": "held", "times": 1, "hold": true })).unwrap();
    let mut answers = daemon.query("echo", dir, held).unwrap();
    assert_eq!(answers.next().unwrap().unwrap(), update("held"));
    answers.canceller().cancel().unwrap();
    assert_eq!(
        answers.next().unwrap().unwrap(),
        Answer::Reply(Outcome::Interrupted { output: None })
    );
    assert!(answers.next().is_none());

    assert_eq!(env.logs(), before, "a query changed a log");
}
