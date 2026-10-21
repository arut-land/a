#![cfg(feature = "fixture")]

use ext::host::Registry;
use ext::testing::Rig;
use ext::{Extension, Replay};
use ext_mcp::{Failure, Mcp, Server, Transport};
use serde_json::{Value, json};
use std::time::Duration;

fn rig(name: &str, timeout: Duration) -> Rig {
    let dir = std::env::temp_dir().join(format!("a-rig-mcp-{name}-{}", std::process::id()));
    let mcp = Mcp::new(
        vec![Server {
            name: "fixture".into(),
            description: "test server".into(),
            transport: Transport::Stdio(vec![
                env!("CARGO_BIN_EXE_mcp-fixture").into(),
                dir.join("effects").display().to_string(),
            ]),
            env: std::collections::BTreeMap::default(),
        }],
        timeout,
    )
    .unwrap();
    Rig::new(
        &format!("mcp-{name}"),
        Registry::default()
            .with(&ext_shell::Shell)
            .unwrap()
            .with(&mcp)
            .unwrap(),
    )
}

fn failed(rig: &Rig, tool: &str) -> Failure {
    serde_json::from_value(
        rig.done(
            "mcp_call",
            &json!({"server":"fixture","tool":tool,"arguments":{}}),
        )["failed"]
            .clone(),
    )
    .unwrap()
}

fn finish(rig: &Rig) {
    // Rig records process ownership; the real engine supplies its cleanup.
    for (pid, _) in rig.spawned.lock().unwrap().iter() {
        let _ = sys::Group(*pid).kill();
    }
    rt::block_on(async {
        let reactor = rt::Reactor::AsyncIo;
        for _ in 0..200 {
            if rig
                .spawned
                .lock()
                .unwrap()
                .iter()
                .all(|(p, _)| !sys::Group(*p).alive())
            {
                return;
            }
            reactor.sleep(Duration::from_millis(10)).await;
        }
        panic!("fixture process leaked");
    });
}

#[test]
fn discovery_is_lazy_and_only_selected_schemas_reach_the_model() {
    let rig = rig("lazy", Duration::from_secs(10));
    assert!(!rig.dir.join("effects").exists());
    assert!(
        rig.registry
            .system_prompt()
            .contains("fixture: test server")
    );
    assert!(!rig.registry.system_prompt().contains("inputSchema"));
    let list = rig.done("mcp_list", &json!({"server":"fixture"}));
    assert_eq!(list["ok"][0]["name"], "echo");
    assert!(list["ok"][0].get("inputSchema").is_none());
    let schema = rig.done("mcp_schema", &json!({"server":"fixture","tool":"echo"}));
    assert_eq!(schema["ok"]["inputSchema"]["type"], "object");
    let result = rig.done(
        "mcp_call",
        &json!({"server":"fixture","tool":"echo","arguments":{"text":"hello"}}),
    );
    assert!(
        result["ok"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hello")
    );
    assert_eq!(
        std::fs::read_to_string(rig.dir.join("effects"))
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("start"))
            .count(),
        1
    );
    finish(&rig);
}

#[test]
fn crashes_and_protocol_failures_are_logged_as_distinct_facts() {
    let rig = rig("failures", Duration::from_secs(10));
    assert_eq!(failed(&rig, "crash"), Failure::Crashed);
    assert!(matches!(failed(&rig, "bad"), Failure::Protocol(_)));
    assert!(matches!(failed(&rig, "wrong-id"), Failure::Protocol(_)));
    assert_eq!(
        failed(&rig, "remote"),
        Failure::Remote {
            code: -32602,
            message: "bad arguments".into()
        }
    );
    assert_eq!(failed(&rig, "input"), Failure::InputRequired);
    finish(&rig);
}

#[test]
fn timeout_kills_the_request_and_a_later_call_gets_a_new_process() {
    let rig = rig("timeout", Duration::from_secs(1));
    assert_eq!(failed(&rig, "wait"), Failure::Timeout);
    let result = rig.done(
        "mcp_call",
        &json!({"server":"fixture","tool":"echo","arguments":{}}),
    );
    assert!(result.get("ok").is_some());
    finish(&rig);
}

#[test]
fn cancellation_kills_the_process_even_when_the_server_ignores_it() {
    let rig = rig("cancel", Duration::from_secs(10));
    let cx = rig.cx();
    let input =
        protocol::Json::encode(&json!({"server":"fixture","tool":"wait","arguments":{}})).unwrap();
    rt::block_on(async {
        let call = async {
            rig.registry
                .tools()
                .run("mcp_call", &input, None, &cx)
                .await
        };
        let cancel = async {
            let reactor = rt::Reactor::AsyncIo;
            while !rig.dir.join("effects.ready").exists() {
                reactor.sleep(Duration::from_millis(10)).await;
            }
            rig.cancel.set();
            std::future::pending().await
        };
        let called = rt::or(call, cancel).await;
        let protocol::Outcome::Done { output } = called.outcome else {
            panic!("no typed output")
        };
        let value: Value = output.parse().unwrap();
        assert_eq!(value["failed"], "cancelled");
    });
    assert_eq!(*rig.progress.lock().unwrap(), "waiting");
    finish(&rig);
}

#[test]
fn every_mcp_effect_is_unsafe_to_replay_and_policy_gates_before_start() {
    let rig = rig("policy", Duration::from_secs(10));
    for name in ["mcp_list", "mcp_schema", "mcp_call"] {
        assert_eq!(
            rig.registry.tools().get(name).unwrap().replay(),
            Replay::Unsafe
        );
    }
    let policy = ext_policy::Policy::ask(&["mcp_call"]);
    let registry = Registry::default().with(&policy).unwrap();
    let cx = rig.cx();
    let input =
        protocol::Json::encode(&json!({"server":"fixture","tool":"echo","arguments":{}})).unwrap();
    let gated = registry
        .hooks()
        .gate(&cx.point(), "mcp_call", &input)
        .now()
        .unwrap();
    assert_eq!(gated.gate, ext::Gate::Ask);
    assert!(!rig.dir.join("effects").exists());
    assert_eq!(<Mcp as Extension>::NAME, "mcp");
}

#[test]
fn notification_flood_is_bounded_per_request() {
    let rig = rig("review-flood", Duration::from_secs(10));
    let result = rig.done(
        "mcp_call",
        &json!({"server":"fixture","tool":"flood","arguments":{}}),
    );
    let progress_bytes = rig.progress.lock().unwrap().len();
    finish(&rig);
    assert!(progress_bytes <= 8 * 1024);
    assert_eq!(
        result["failed"], "limit",
        "accepted {progress_bytes} progress bytes: {result}"
    );
}

#[test]
fn progress_bursts_publish_a_bounded_latest_value() {
    let rig = rig("progress-burst", Duration::from_secs(10));
    let result = rig.done(
        "mcp_call",
        &json!({"server":"fixture","tool":"progress-burst","arguments":{}}),
    );
    assert!(result.get("ok").is_some());
    let text = rig.progress.lock().unwrap().clone();
    assert!(text.starts_with("update 0\n"));
    assert!(text.ends_with("update 99\n"));
    assert!(text.lines().count() <= 8);
    finish(&rig);
}

