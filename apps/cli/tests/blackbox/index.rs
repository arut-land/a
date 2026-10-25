//! The workspace index through the client SDK against the built daemon:
//! `files.search` and `files.grep` about a workspace no session has
//! been started in.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use client::{Answer, Daemon};
use protocol::{Json, Outcome, Scope};
use serde_json::{Value, json};

use crate::support::{Isolated, deadline};

/// A daemon (started by a session in the home's root) and a git working
/// tree beside it that nothing has asked about yet.
fn started(name: &str) -> (Isolated, Daemon, PathBuf) {
    let env = Isolated::new(name);
    env.run(&["-p", "hello"]);
    let staged = (env.home.runtime())
        .stage(Path::new(env!("CARGO_BIN_EXE_a")))
        .unwrap();
    let daemon = Daemon::local(env.home.clone(), staged);
    let tree = env.home.root().join("tree");
    for (path, content) in [
        (".git/HEAD", "ref: refs/heads/main\n"),
        (".gitignore", "target/\n"),
        ("src/main.rs", "fn main() {\n    serve();\n}\n"),
        ("src/server.rs", "pub fn serve() {}\n"),
        ("target/debug/main.rs", "fn main() {}\n"),
    ] {
        let path = tree.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    (env, daemon, tree)
}

fn ask(daemon: &Daemon, service: &str, tree: &Path, body: &Value) -> Vec<Answer> {
    let scope = Scope::Workspace(tree.to_path_buf().try_into().unwrap());
    let body = Json::encode(body).unwrap();
    let answers = daemon.query(service, scope, body).unwrap();
    answers.map(Result::unwrap).collect()
}

fn value(json: &Json) -> Value {
    serde_json::from_str(&json.to_string()).unwrap()
}

fn reply(answers: &[Answer]) -> Value {
    match answers.last() {
        Some(Answer::Reply(Outcome::Done { output })) => value(output),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_at_mention_search_answers_from_the_workspace_index() {
    let (_env, daemon, tree) = started("index-search");
    let found = reply(&ask(
        &daemon,
        "files.search",
        &tree,
        &json!({ "query": "main" }),
    ));
    let paths: Vec<&str> = (found["files"].as_array().unwrap().iter())
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths.first(), Some(&"src/main.rs"), "{found}");
    assert!(!paths.iter().any(|p| p.starts_with("target/")), "{found}");

    // A file created since is found, the watcher having seen it.
    fs::write(tree.join("src/router.rs"), "pub fn route() {}\n").unwrap();
    let until = deadline();
    loop {
        let found = reply(&ask(
            &daemon,
            "files.search",
            &tree,
            &json!({ "query": "router" }),
        ));
        if found["files"][0]["path"] == "src/router.rs" {
            break;
        }
        assert!(Instant::now() < until, "{found}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn a_grep_streams_each_files_matches_then_its_summary() {
    let (_env, daemon, tree) = started("index-grep");
    let answers = ask(&daemon, "files.grep", &tree, &json!({ "query": "serve" }));
    let mut files: Vec<String> = (answers.iter())
        .filter_map(|answer| match answer {
            Answer::Update(update) => Some(value(update)["path"].as_str().unwrap().to_owned()),
            Answer::Reply(_) => None,
        })
        .collect();
    files.sort();
    assert_eq!(files, ["src/main.rs", "src/server.rs"]);
    let summary = reply(&answers);
    assert_eq!(summary["matches"], 2, "{summary}");
    assert_eq!(summary["truncated"], false, "{summary}");
}
