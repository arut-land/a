//! The workspace index through the client SDK against the built daemon:
//! `files.search` and `files.grep` about a workspace no session has
//! been started in.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

fn ask(env: &Isolated, daemon: &Daemon, service: &str, tree: &Path, body: &Value) -> Vec<Answer> {
    let scope = Scope::Workspace(tree.to_path_buf().try_into().unwrap());
    let body = Json::encode(body).unwrap();
    // A daemon that went away says why in its log.
    let failed = |error: &dyn std::fmt::Display| -> ! {
        let mut said = String::new();
        for dir in [env.home.logs_dir(), env.home.crash_dir()] {
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let text = fs::read_to_string(entry.path()).unwrap_or_default();
                let _ = writeln!(said, "{}:\n{text}", entry.path().display());
            }
        }
        let status = env
            .command(&["daemon", "status"])
            .output()
            .map(|o| o.stdout);
        let status = String::from_utf8_lossy(&status.unwrap_or_default()).into_owned();
        panic!("{service}: {error}\ndaemon status: {status}\n{said}")
    };
    let answers = daemon
        .query(service, scope, body)
        .unwrap_or_else(|e| failed(&e));
    answers
        .map(|answer| answer.unwrap_or_else(|e| failed(&e)))
        .collect()
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
fn the_first_grep_after_restart_reconciles_an_index_created_only_on_query() {
    let env = Isolated::new("index-persist");
    env.run(&["ls"]);
    let cache = env.home.cache_dir().join("index");
    assert!(!cache.exists());
    let tree = env.home.root().join("tree");
    fs::create_dir_all(tree.join(".git")).unwrap();
    let file = tree.join("item.txt");
    fs::write(&file, "oldneedle\n").unwrap();
    let staged = env
        .home
        .runtime()
        .stage(Path::new(env!("CARGO_BIN_EXE_a")))
        .unwrap();
    let daemon = Daemon::local(env.home.clone(), staged);
    let answers = ask(
        &env,
        &daemon,
        "files.grep",
        &tree,
        &json!({"query": "oldneedle"}),
    );
    assert_eq!(reply(&answers)["matches"], 1);

    let until = Instant::now() + Duration::from_secs(60);
    while !fs::read_dir(&cache)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "files")
        })
    {
        assert!(Instant::now() < until, "the queried index never persisted");
        std::thread::sleep(Duration::from_millis(100));
    }
    env.run(&["daemon", "stop"]);
    fs::write(&file, "newneedle\n").unwrap();
    env.run(&["ls"]);
    let staged = env
        .home
        .runtime()
        .stage(Path::new(env!("CARGO_BIN_EXE_a")))
        .unwrap();
    let daemon = Daemon::local(env.home.clone(), staged);
    let answers = ask(
        &env,
        &daemon,
        "files.grep",
        &tree,
        &json!({"query": "newneedle"}),
    );
    assert_eq!(reply(&answers)["matches"], 1);
}

#[test]
fn an_at_mention_search_answers_from_the_workspace_index() {
    let (env, daemon, tree) = started("index-search");
    let found = reply(&ask(
        &env,
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
            &env,
            &daemon,
            "files.search",
            &tree,
            &json!({ "query": "router" }),
        ));
        if found["files"][0]["path"] == "src/router.rs" {
            break;
        }
        assert!(Instant::now() < until, "{found}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_grep_streams_each_files_matches_then_its_summary() {
    let (env, daemon, tree) = started("index-grep");
    let answers = ask(
        &env,
        &daemon,
        "files.grep",
        &tree,
        &json!({ "query": "serve" }),
    );
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

#[test]
fn a_grep_cancelled_mid_stream_is_interrupted_and_reads_no_further() {
    let (_env, daemon, tree) = started("index-cancel");
    // More files with a match than the connection holds updates for: the
    // grep waits on the client, so it is still running when cancelled.
    for i in 0..3000 {
        let path = tree.join(format!("many/{i}.txt"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "needle\n").unwrap();
    }
    let scope = Scope::Workspace(tree.clone().try_into().unwrap());
    let body = Json::encode(&json!({ "query": "needle", "limit": 100_000 })).unwrap();
    let mut answers = daemon.query("files.grep", scope, body).unwrap();
    assert!(matches!(
        answers.next().unwrap().unwrap(),
        Answer::Update(_)
    ));
    answers.canceller().cancel().unwrap();
    // Updates already on their way may come first; the reply ends it.
    let rest: Vec<Answer> = answers.map(Result::unwrap).collect();
    assert_eq!(
        rest.last(),
        Some(&Answer::Reply(Outcome::Interrupted { output: None }))
    );
    assert!(rest.len() < 3000, "{} answers after the cancel", rest.len());
}
