#![cfg(feature = "fixture")]

use ext::host::Registry;
use ext::testing::Rig;
use ext::{Extension, Replay};
use ext_mcp::{Failure, Mcp, Server, Transport};
use serde_json::{Value, json};
use std::time::Duration;

fn rig(name: &str, timeout: Duration) -> Rig {
    let mcp = Mcp::new(
        vec![Server {
            name: "fixture".parse().unwrap(),
            description: "test server".into(),
            transport: Transport::Stdio(vec![
                env!("CARGO_BIN_EXE_mcp-fixture").into(),
                format!("effects-{name}"),
            ]),
            env: std::collections::BTreeMap::default(),
        }],
        timeout,
    )
    .unwrap();
    Rig::new(
        &format!("mcp-{name}"),
        Registry::default()
            .with(&ext_shell::Shell::default())
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

fn finish(rig: Rig) {
    let recorded = rig.spawned.lock().unwrap().clone();
    drop(rig);
    assert!(
        recorded
            .iter()
            .all(|(pid, _)| !sys::RecordedIdentity(*pid).alive())
    );
}

#[test]
fn discovery_is_lazy_and_only_selected_schemas_reach_the_model() {
    let rig = rig("lazy", Duration::from_secs(10));
    assert!(!rig.workspace().join("effects-lazy").exists());
    assert!(
        rig.registry
            .generation_context()
            .unwrap()
            .contains("fixture: test server")
    );
    assert!(
        !rig.registry
            .generation_context()
            .unwrap()
            .contains("inputSchema")
    );
    let list = rig.done("mcp_list", &json!({"server":"fixture"}));
    assert_eq!(list["ok"][0]["name"], "echo");
    assert!(list["ok"][0].get("inputSchema").is_none());
    let schema = rig.done("mcp_schema", &json!({"server":"fixture","tool":"echo"}));
    assert_eq!(schema["ok"]["loaded"], true);
    let log = rig.log.lock().unwrap();
    let definition = log
        .iter()
        .find_map(|e| match &e.body {
            protocol::Body::Context {
                text,
                publication: Some(p),
            } if p.namespace == "mcp-schemas" => {
                Some(serde_json::from_str::<Value>(text.split_once('\n').unwrap().1).unwrap())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(definition["inputSchema"]["type"], "object");
    drop(log);
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
        std::fs::read_to_string(rig.workspace().join("effects-lazy"))
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("start"))
            .count(),
        1
    );
    finish(rig);
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
    finish(rig);
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
    finish(rig);
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
            while rig.progress.lock().unwrap().as_str() != "waiting" {
                reactor.sleep(Duration::from_millis(10)).await;
            }
            rig.cancel.set();
            std::future::pending().await
        };
        let called = rt::or(call, cancel).await;
        let protocol::Outcome::Done {
            output: protocol::Payload::Inline(output),
        } = called.outcome
        else {
            panic!("no typed output")
        };
        let value: Value = output.parse().unwrap();
        assert_eq!(value["failed"], "cancelled");
    });
    assert_eq!(*rig.progress.lock().unwrap(), "waiting");
    finish(rig);
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
    assert!(!rig.workspace().join("effects-policy").exists());
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
    finish(rig);
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
    finish(rig);
}

#[test]
fn paginated_discovery_cannot_exceed_the_client_frame_budget() {
    let rig = rig("review-large-list", Duration::from_secs(10));
    let result = rig.done("mcp_list", &json!({"server":"fixture"}));
    finish(rig);
    assert_eq!(result["failed"], "limit");
    let bytes = serde_json::to_vec(&result).unwrap().len();
    assert!(
        bytes < 16 * 1024 * 1024,
        "MCP list produced {bytes} bytes before entry/card framing"
    );
}

#[test]
fn selected_schemas_and_compact_indexes_fit_with_their_escaped_cards() {
    let rig = rig("bounded-escaped", Duration::from_secs(10));
    let list = rig.done("mcp_list", &json!({"server":"fixture"}));
    assert!(list["ok"][0]["description"].as_str().unwrap().len() <= 512);
    let cx = rig.cx();
    let input = protocol::Json::encode(&json!({"server":"fixture","tool":"echo"})).unwrap();
    let called = rt::block_on(rig.registry.tools().run("mcp_schema", &input, None, &cx));
    assert!(matches!(called.outcome, protocol::Outcome::Done { .. }));
    let entry = protocol::Entry::new(
        protocol::Seq::new(2).unwrap(),
        protocol::Epoch(1),
        protocol::Body::ToolResult {
            call: cx.call,
            outcome: called.outcome,
            card: called.card,
            held: Vec::new(),
        },
    );
    for publication in rig.log.lock().unwrap().iter() {
        let frame = protocol::Frame::entry(publication).unwrap();
        assert!(frame.as_bytes().len() < 4 * 1024 * 1024);
        assert!(protocol::frame_in(frame.as_bytes()).unwrap().is_some());
    }
    let frame = protocol::Frame::entry(&entry).unwrap();
    assert!(frame.as_bytes().len() < 4 * 1024 * 1024);
    assert!(protocol::frame_in(frame.as_bytes()).unwrap().is_some());
    finish(rig);
}

#[test]
fn invalid_discovered_identities_are_refused_before_publication() {
    let rig = rig("invalid-name", Duration::from_secs(10));
    let result = rig.done("mcp_list", &json!({"server":"fixture"}));
    assert!(result["failed"].get("protocol").is_some());
    finish(rig);
}

#[test]
fn coalesced_progress_is_published_even_when_the_peer_goes_quiet() {
    let rig = rig("progress-pause", Duration::from_secs(5));
    let cx = rig.cx();
    let input =
        protocol::Json::encode(&json!({"server":"fixture","tool":"progress-pause","arguments":{}}))
            .unwrap();
    let called = rt::block_on(async {
        rt::or(
            rig.registry.tools().run("mcp_call", &input, None, &cx),
            async {
                let reactor = rt::Reactor::AsyncIo;
                while !rig.progress.lock().unwrap().ends_with("latest\n") {
                    reactor.sleep(Duration::from_millis(5)).await;
                }
                rig.cancel.set();
                std::future::pending().await
            },
        )
        .await
    });
    let protocol::Outcome::Done {
        output: protocol::Payload::Inline(output),
    } = called.outcome
    else {
        panic!("no typed result")
    };
    let fact: Value = output.parse().unwrap();
    assert_eq!(fact["failed"], "cancelled");
    assert!(rig.progress.lock().unwrap().lines().count() <= 8);
    finish(rig);
}

#[test]
fn schema_publications_follow_effective_context_across_restart_and_edits() {
    let rig = rig("publications", Duration::from_secs(10));
    let input = json!({"server":"fixture","tool":"echo"});
    assert_eq!(
        rig.done("mcp_schema", &input)["ok"]["already_loaded"],
        false
    );
    rig.restart();
    assert_eq!(rig.done("mcp_schema", &input)["ok"]["already_loaded"], true);
    let target = rig
        .log
        .lock()
        .unwrap()
        .iter()
        .find(|e| matches!(e.body, protocol::Body::Context { .. }))
        .unwrap()
        .seq;
    rig.cx().edit(target, None, None);
    assert_eq!(
        rig.done("mcp_schema", &input)["ok"]["already_loaded"],
        false
    );
    let count = rig.log.lock().unwrap().iter().filter(|e| matches!(&e.body, protocol::Body::Context { publication: Some(p), .. } if p.namespace == "mcp-schemas")).count();
    assert_eq!(count, 2);
    finish(rig);
}
