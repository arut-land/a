//! MCP effects use the same logged intents, approvals and recovery as
//! built-in tools; replay and idle startup never reach a server.
use crate::common::{Env, wait_for};
use protocol::{Body, ClientMsg, Outcome, Permission, Target, Verdict};
use serde_json::json;
use std::fs;

fn configured(name: &str) -> Env {
    let env = Env::new(name);
    env.daemon("crash_limit = 0");
    let fixture = std::path::Path::new(env!("CARGO_BIN_EXE_a")).with_file_name("mcp-fixture");
    assert!(
        fixture.exists(),
        "build the workspace with MCP fixture support"
    );
    env.ext(
        "mcp",
        &format!(
            "servers = [\"fixture\"]\nfixture = [{:?}, {:?}]",
            fixture.to_str().unwrap(),
            env.root.join("effects").to_str().unwrap()
        ),
    );
    env
}

fn call(conn: &mut crate::common::Conn, tool: &str) {
    conn.input(&format!(
        "call mcp_call {}",
        json!({"server":"fixture","tool":tool,"arguments":{"text":"hello"}})
    ));
}

fn pid(env: &Env) -> u32 {
    fs::read_to_string(env.root.join("effects"))
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("start "))
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn replay_never_repeats_a_server_effect_and_stop_owns_its_tree() {
    let env = configured("mcp-replay");
    env.start();
    let mut conn = env.connect();
    let (session, _) = conn.attach(Target::New, &env.ws);
    conn.input("hello");
    conn.until(|e| matches!(&e.body, Body::Assistant { text } if text == "hello"));
    assert!(!env.root.join("effects").exists());
    call(&mut conn, "echo");
    let entries = conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    assert!(entries.iter().any(|e| matches!(
        &e.body,
        Body::ToolResult {
            outcome: Outcome::Done { .. },
            ..
        }
    )));
    let child = pid(&env);
    let effects = fs::read(env.root.join("effects")).unwrap();
    env.run(&["daemon", "stop"]);
    wait_for("MCP child to end with its daemon", || {
        !sys::RecordedIdentity(child).alive()
    });
    env.start();
    let mut replay = env.connect();
    let (_, entries) = replay.attach(Target::Id(session), &env.ws);
    assert!(
        entries
            .iter()
            .any(|e| matches!(e.body, Body::ToolResult { .. }))
    );
    replay.input("resumed");
    replay.until(|e| matches!(&e.body, Body::Assistant { text } if text == "resumed"));
    assert_eq!(fs::read(env.root.join("effects")).unwrap(), effects);
}

#[test]
fn an_unfinished_external_call_is_never_reissued_after_daemon_crash() {
    let env = configured("mcp-crash");
    env.start();
    let mut conn = env.connect();
    let (session, _) = conn.attach(Target::New, &env.ws);
    call(&mut conn, "wait");
    conn.until(|e| matches!(&e.body, Body::Progress { .. }));
    let child = pid(&env);
    let effects = fs::read(env.root.join("effects")).unwrap();
    env.kill_daemon();
    env.start();
    let mut replay = env.connect();
    let (_, mut entries) = replay.attach(Target::Id(session), &env.ws);
    replay.input("resumed");
    entries
        .extend(replay.until(|e| matches!(&e.body, Body::Assistant { text } if text == "resumed")));
    assert!(entries.iter().any(|e| matches!(
        &e.body,
        Body::ToolResult {
            outcome: Outcome::Interrupted { .. },
            ..
        }
    )));
    wait_for("recovery to kill the old server", || {
        !sys::RecordedIdentity(child).alive()
    });
    assert_eq!(fs::read(env.root.join("effects")).unwrap(), effects);
}

#[test]
fn approval_and_cancellation_precede_any_remote_effect() {
    let env = configured("mcp-approval");
    env.ext("policy", "ask = [\"mcp_call\"]");
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    call(&mut conn, "echo");
    conn.until(|e| {
        matches!(
            &e.body,
            Body::Decision {
                d: Verdict::Permission {
                    permission: Permission::Ask(_)
                },
                ..
            }
        )
    });
    assert!(!env.root.join("effects").exists());
    conn.send(&ClientMsg::INTERRUPT);
    conn.until(|e| matches!(&e.body, Body::ToolResult { .. }));
    assert!(!env.root.join("effects").exists());
}

#[test]
fn an_ordinary_first_turn_starts_zero_of_zero_or_three_configured_servers() {
    for count in [0, 3] {
        let env = Env::new(if count == 0 {
            "mcp-first-zero"
        } else {
            "mcp-first-three"
        });
        if count == 3 {
            let fixture =
                std::path::Path::new(env!("CARGO_BIN_EXE_a")).with_file_name("mcp-fixture");
            let mut settings = String::from("servers = [\"one\", \"two\", \"three\"]\n");
            for name in ["one", "two", "three"] {
                use std::fmt::Write;
                writeln!(
                    settings,
                    "{name} = [{:?}, {:?}]",
                    fixture.to_str().unwrap(),
                    env.root.join("effects").to_str().unwrap()
                )
                .unwrap();
            }
            env.ext("mcp", &settings);
        }
        env.start();
        let mut conn = env.connect();
        conn.attach(Target::New, &env.ws);
        conn.input("first turn");
        conn.until(|e| matches!(&e.body, Body::Assistant { text } if text == "first turn"));
        assert!(!env.root.join("effects").exists());
        eprintln!(
            "{count} configured MCP servers: first turn completed, 0 servers started; latency not measured on shared host"
        );
    }
}

#[test]
fn interrupting_an_active_call_kills_its_server() {
    let env = configured("mcp-interrupt");
    env.start();
    let mut conn = env.connect();
    conn.attach(Target::New, &env.ws);
    call(&mut conn, "wait");
    conn.until(|e| matches!(e.body, Body::Progress { .. }));
    let child = pid(&env);
    conn.send(&ClientMsg::INTERRUPT);
    let entries = conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    match &entries.last().unwrap().body {
        Body::ToolResult {
            outcome: Outcome::Interrupted { .. },
            ..
        } => {}
        Body::ToolResult {
            outcome: Outcome::Done { output },
            ..
        } => {
            // The tool can observe cancellation before the driver's race;
            // either winner must durably record that no reply was obtained.
            let fact: serde_json::Value = output.parse().unwrap();
            assert_eq!(fact["failed"], "cancelled");
        }
        other => panic!("expected a cancelled call, got {other:?}"),
    }
    wait_for("interrupted MCP server to exit", || {
        !sys::RecordedIdentity(child).alive()
    });
}

#[test]
fn daemon_stop_and_cancellation_kill_server_descendants() {
    for cancel in [false, true] {
        let env = configured(if cancel {
            "mcp-tree-cancel"
        } else {
            "mcp-tree-stop"
        });
        env.start();
        let mut conn = env.connect();
        conn.attach(Target::New, &env.ws);
        call(&mut conn, if cancel { "tree-wait" } else { "tree" });
        conn.until(|e| {
            if cancel {
                matches!(e.body, Body::Progress { .. })
            } else {
                matches!(e.body, Body::ToolResult { .. })
            }
        });
        let server = pid(&env);
        let child: u32 = fs::read_to_string(env.root.join("effects"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("descendant "))
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(server, child);
        assert!(sys::RecordedIdentity(server).members().contains(&child));
        if cancel {
            conn.send(&ClientMsg::INTERRUPT);
            conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
        } else {
            env.run(&["daemon", "stop"]);
        }
        wait_for(
            "the server and its descendant to leave their process group",
            || !sys::RecordedIdentity(server).alive(),
        );
        assert!(!sys::RecordedIdentity(server).members().contains(&child));
    }
}

/// An MCP server is a helper, not the user's work: it holds no upgrade.
/// The session's job goes to the newer build live, the server is stopped
/// with the older one, and the newer build starts it again on the next
/// call that needs it.
#[test]
fn an_mcp_server_never_holds_an_upgrade_and_starts_again_after_it() {
    let env = configured("mcp-upgrade");
    let old = env.older_build();
    assert!(env.cmd_as(&old, &["ls"]).status().unwrap().success());
    let pid = env.pid().unwrap();
    let mut conn = env.connect();
    let (session, _) = conn.attach(Target::New, &env.ws);
    call(&mut conn, "echo");
    conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    let server = self::pid(&env);
    conn.input("$ sleep 30 & echo job");
    conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    drop(conn);

    env.run(&["ls"]);
    assert_eq!(env.pid(), Some(pid), "the job was not handed over");
    wait_for("the older build's MCP server to stop", || {
        !sys::RecordedIdentity(server).alive()
    });
    let mut conn = env.connect();
    conn.attach(Target::Id(session), &env.ws);
    call(&mut conn, "echo");
    let entries = conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    assert!(entries.iter().any(|e| matches!(
        &e.body,
        Body::ToolResult {
            outcome: Outcome::Done { .. },
            ..
        }
    )));
    let starts = fs::read_to_string(env.root.join("effects")).unwrap();
    assert_eq!(
        starts.lines().filter(|l| l.starts_with("start ")).count(),
        2,
        "{starts}"
    );
}
