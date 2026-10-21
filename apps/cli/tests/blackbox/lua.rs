use std::fs;
use std::path::Path;

use crate::common::Env;
use protocol::{Body, Outcome, Target};
use serde_json::{Value, json};

fn write(dir: &Path, name: &str, source: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(format!("{name}.lua")), source).unwrap();
}

fn command(conn: &mut crate::common::Conn, text: &str) -> Outcome {
    conn.input(text);
    let entries = conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    match &entries.last().unwrap().body {
        Body::ToolResult { outcome, .. } => outcome.clone(),
        other => panic!("{other:?}"),
    }
}

fn done(conn: &mut crate::common::Conn, text: &str) -> Value {
    match command(conn, text) {
        Outcome::Done { output } => output.parse().unwrap(),
        other => panic!("{text}: {other:?}"),
    }
}

#[test]
fn lua_tools_run_through_the_echo_agents_durable_calls() {
    let env = Env::new("lua-tool");
    write(
        &env.home.root().join("ext"),
        "greet",
        "a.tool{name='greet',audience='user',description='greet',input={type='object'},run=function(input) return {greeting=input.args} end}",
    );
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    assert_eq!(done(&mut conn, "/greet Ada"), json!({"greeting":"Ada"}));
    assert!(
        env.log(id)
            .iter()
            .any(|e| matches!(&e.body, Body::ToolCall { tool, .. } if tool=="greet"))
    );
}

#[test]
fn lua_input_handlers_run_before_the_echo_agent() {
    let env = Env::new("lua-handler");
    write(
        &env.home.root().join("ext"),
        "rewrite",
        "a.on('input',function(input) return {replace='rewritten'} end)",
    );
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    conn.input("original");
    let entries = conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    assert!(
        matches!(&entries.last().unwrap().body, Body::Assistant { text, .. } if text=="rewritten")
    );
}

#[test]
fn workspace_mods_replace_home_mods_and_rust_tools_by_name() {
    let env = Env::new("lua-override");
    env.ext("policy", "mode='auto'");
    let source = |origin| {
        format!(
            "a.tool{{name='bash',description='override',input={{type='object'}},run=function() return {{source='{origin}'}} end}}"
        )
    };
    write(&env.home.root().join("ext"), "override", &source("home"));
    write(&env.ws.join(".a/ext"), "override", &source("workspace"));
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    assert_eq!(conn.run_command("ignored"), json!({"source":"workspace"}));
}

#[test]
fn crashing_and_looping_mods_leave_the_daemon_usable() {
    let env = Env::new("lua-failures");
    write(
        &env.home.root().join("ext"),
        "policy-floor",
        "a.text{name='attempt',text='replace guarded'}",
    );
    write(
        &env.home.root().join("ext"),
        "invalid",
        "error('bad registration')",
    );
    write(
        &env.home.root().join("ext"),
        "broken",
        "a.tool{name='crash',audience='user',description='crash',input={type='object'},run=function() error('broken') end}; a.tool{name='loop',audience='user',description='loop',input={type='object'},run=function() while true do end end}",
    );
    let pid = env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    assert!(
        matches!(command(&mut conn,"/crash"), Outcome::Failed { error } if error.contains("broken"))
    );
    assert!(
        matches!(command(&mut conn,"/loop"), Outcome::Failed { error } if error.contains("budget"))
    );
    conn.input("still alive");
    let entries = conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    assert!(
        matches!(&entries.last().unwrap().body, Body::Assistant { text, .. } if text=="still alive")
    );
    assert_eq!(env.pid(), Some(pid));
}

#[test]
fn file_reload_preserves_folded_state_and_rejects_a_broken_generation() {
    let env = Env::new("lua-reload");
    let dir = env.home.root().join("ext");
    let source = |n| {
        format!(
            "a.fold{{name='count',initial=0,apply=function(state,change) return state+change end}}; a.tool{{name='count',audience='user',description='count',input={{type='object'}},run=function(input,cx) cx.update('count',{n}); return cx.state('count') end}}"
        )
    };
    write(&dir, "counter", &source(1));
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    assert_eq!(done(&mut conn, "/count"), 1);
    write(&dir, "counter", &source(3));
    assert_eq!(done(&mut conn, "/lua-reload"), true);
    assert_eq!(done(&mut conn, "/count"), 4);
    write(&dir, "counter", "this is not lua!");
    assert!(matches!(
        command(&mut conn, "/lua-reload"),
        Outcome::Failed { .. }
    ));
    assert_eq!(done(&mut conn, "/count"), 7);
    write(&dir, "counter", &source(2));
    assert_eq!(done(&mut conn, "/lua-reload"), true);
    assert_eq!(done(&mut conn, "/count"), 9);
    drop(conn);
    assert!(env.cmd(&["daemon", "stop"]).status().unwrap().success());
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::Id(id), &env.ws);
    assert_eq!(done(&mut conn, "/count"), 11);
}

#[test]
fn lua_shell_delegation_requires_a_grant_and_obeys_the_policy_floor() {
    let env = Env::new("lua-shell");
    env.ext("runner", "tools=['bash']");
    env.ext("policy", "mode='auto'");
    write(
        &env.home.root().join("ext"),
        "runner",
        "a.tool{name='run',audience='user',description='run',input={type='object'},run=function(input,cx) return cx.tool('run','bash',{command='printf lua'}) end}",
    );
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    assert_eq!(done(&mut conn, "/run")["output"], "lua");
    drop(conn);
    assert!(env.cmd(&["daemon", "stop"]).status().unwrap().success());
    fs::write(
        env.home.policy_file(),
        "[[rule]]\ntool='bash'\neffect='deny'\n",
    )
    .unwrap();
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    assert!(
        matches!(command(&mut conn,"/run"), Outcome::Failed { error } if error.contains("denied"))
    );
}

#[cfg(feature = "fork")]
#[test]
fn a_historical_fork_keeps_its_lua_generation_and_fold_across_restart() {
    let env = Env::new("lua-fork");
    let dir = env.home.root().join("ext");
    let source = |n| {
        format!(
            "a.fold{{name='count',initial=0,apply=function(s,c) return s+c end}}; a.tool{{name='count',audience='user',description='count',input={{type='object'}},run=function(input,cx) cx.update('count',{n}); return cx.state('count') end}}"
        )
    };
    write(&dir, "counter", &source(1));
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    assert_eq!(done(&mut conn, "/count"), 1);
    let boundary = env.log(id).last().unwrap().seq;
    write(&dir, "counter", &source(3));
    assert_eq!(done(&mut conn, "/lua-reload"), true);
    assert_eq!(done(&mut conn, "/count"), 4);
    let before = env.log(id);
    let fork = env.run(&["fork", &format!("{id}@{boundary}")]);
    let fork = fork.trim();
    let mut branch = env.connect();
    branch.attach(Target::Prefix(fork.to_owned()), &env.ws);
    assert_eq!(done(&mut branch, "/count"), 2);
    assert_eq!(env.log(id), before);
    drop((conn, branch));
    assert!(env.cmd(&["daemon", "stop"]).status().unwrap().success());
    env.start();
    let mut branch = env.connect();
    branch.attach(Target::Prefix(fork.to_owned()), &env.ws);
    assert_eq!(done(&mut branch, "/count"), 3);
}
