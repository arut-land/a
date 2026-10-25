use std::fs;
use std::path::Path;

use crate::common::{Conn, Env};
use protocol::{Body, Entry, Outcome, Target};
use serde_json::{Value, json};

fn write(root: &Path, body: &str) {
    let dir = root.join("skills/review");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: review\ndescription: Review a change\n---\n{body}"),
    )
    .unwrap();
}

fn command(conn: &mut Conn, text: &str) -> (Value, Vec<Entry>) {
    conn.input(text);
    let entries = conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    let Body::ToolResult {
        outcome: Outcome::Done {
            output: protocol::Payload::Inline(output),
        },
        ..
    } = &entries.last().unwrap().body
    else {
        panic!("{text}: {entries:?}");
    };
    (output.parse().unwrap(), entries)
}

fn bodies(entries: &[Entry]) -> usize {
    entries.iter().filter(|e| matches!(&e.body, Body::Context { text, .. } if text.contains("private skill instructions"))).count()
}

#[test]
fn skill_commands_publish_once_and_daemon_replay_keeps_the_recorded_body() {
    let env = Env::new("skills-replay");
    write(&env.ws.join(".a"), "private skill instructions");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    let (output, entries) = command(&mut conn, "/review");
    assert_eq!(output, json!({"Ok":"loaded"}));
    assert_eq!(bodies(&entries), 1);
    assert_eq!(
        command(&mut conn, "/review").0,
        json!({"Ok":"already_loaded"})
    );
    assert_eq!(bodies(&env.log(id)), 1);
    drop(conn);
    assert!(env.cmd(&["daemon", "stop"]).status().unwrap().success());
    fs::remove_dir_all(env.ws.join(".a/skills")).unwrap();
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::Id(id), &env.ws);
    conn.input("continue");
    conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    assert_eq!(bodies(&env.log(id)), 1);
}

#[cfg(feature = "lua")]
#[test]
fn explicit_lua_reload_publishes_the_complete_skills_overlay() {
    let env = Env::new("skills-lua");
    write(&env.ws.join(".a"), "private skill instructions");
    let dir = env.home.root().join("ext");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("greet.lua");
    let source = |text| {
        format!(
            "a.tool{{name='greet',audience='user',description='greet',input={{type='object'}},run=function() return '{text}' end}}"
        )
    };
    fs::write(&file, source("old")).unwrap();
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    assert_eq!(command(&mut conn, "/greet").0, "old");
    fs::write(file, source("new")).unwrap();
    assert_eq!(command(&mut conn, "/lua-reload").0, true);
    assert_eq!(command(&mut conn, "/greet").0, "new");
    assert_eq!(command(&mut conn, "/review").0, json!({"Ok":"loaded"}));
}

#[cfg(feature = "fork")]
#[test]
fn a_fork_after_a_skill_load_keeps_it_loaded_and_one_before_does_not() {
    let env = Env::new("skills-fork");
    write(&env.ws.join(".a"), "private skill instructions");
    assert_eq!(env.run(&["-p", "before load"]), "before load\n");
    let mut conn = env.connect();
    let (source, history) = conn.attach(
        Target::Latest {
            any_workspace: false,
        },
        &env.ws,
    );
    let before_load = history.last().unwrap().seq;
    assert_eq!(command(&mut conn, "/review").0, json!({"Ok":"loaded"}));
    let after_load = env.log(source).last().unwrap().seq;
    let original = env.log(source);
    // A fork must retain the historical catalog as well as its publication
    // keys; newer or missing files cannot change the loaded identity.
    drop(conn);
    assert!(env.cmd(&["daemon", "stop"]).status().unwrap().success());
    fs::remove_dir_all(env.ws.join(".a/skills")).unwrap();
    env.start();
    for (boundary, expected, inherited) in [
        (before_load, "loaded", 0),
        (after_load, "already_loaded", 1),
    ] {
        let fork = env.run(&["fork", &format!("{source}@{boundary}")]);
        let mut branch = env.connect();
        let (id, copied) = branch.attach(Target::Prefix(fork.trim().to_owned()), &env.ws);
        assert_eq!(bodies(&copied), inherited);
        assert_eq!(command(&mut branch, "/review").0, json!({"Ok":expected}));
        assert_eq!(bodies(&env.log(id)), 1);
        drop(branch);
    }
    assert_eq!(env.log(source), original);
}
