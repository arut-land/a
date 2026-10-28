#![cfg(feature = "fixture")]

use std::collections::BTreeMap;

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
            env: BTreeMap::default(),
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
    assert!(matches!(failed(&rig, "input"), Failure::Protocol(_)));
    finish(rig);
}

#[test]
fn discovery_startup_failures_preserve_their_typed_classification() {
    use std::future::poll_fn;
    use std::task::Poll;

    for name in ["discover-crash", "discover-version"] {
        let rig = rig(name, Duration::from_secs(10));
        let contexts: Vec<_> = (0..8).map(|_| rig.cx()).collect();
        let input = protocol::Json::encode(&json!({"server":"fixture"})).unwrap();
        let mut calls: Vec<_> = contexts
            .iter()
            .map(|cx| rig.registry.tools().run("mcp_list", &input, None, cx))
            .collect();
        let results = rt::block_on(async {
            poll_fn(|cx| {
                for call in &mut calls {
                    assert!(call.as_mut().poll(cx).is_pending());
                }
                Poll::Ready(())
            })
            .await;
            std::fs::write(
                rig.workspace().join(format!("effects-{name}.release")),
                b"ready",
            )
            .unwrap();
            let mut results = Vec::new();
            for call in calls {
                let protocol::Outcome::Done {
                    output: protocol::Payload::Inline(output),
                } = call.await.outcome
                else {
                    panic!("expected a typed startup failure");
                };
                results.push(output.parse::<Value>().unwrap());
            }
            results
        });
        for result in results {
            let failure: Failure = serde_json::from_value(result["failed"].clone()).unwrap();
            if name == "discover-crash" {
                assert_eq!(failure, Failure::Crashed);
            } else {
                assert!(matches!(failure, Failure::Protocol(_)), "{failure:?}");
            }
        }
        let effects =
            std::fs::read_to_string(rig.workspace().join(format!("effects-{name}"))).unwrap();
        assert_eq!(
            effects
                .lines()
                .filter(|line| line.starts_with("start "))
                .count(),
            1
        );
        assert_eq!(
            effects.lines().filter(|line| *line == "discover").count(),
            1
        );
        // A later explicit request retries after the failed generation is cleaned up.
        let again = rig.done("mcp_list", &json!({"server":"fixture"}));
        assert_eq!(
            again["failed"],
            if name == "discover-crash" {
                json!(Failure::Crashed)
            } else {
                json!(Failure::Protocol(
                    "server does not support MCP 2026-07-28".into()
                ))
            }
        );
        finish(rig);
    }
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
        protocol::Json::encode(&json!({"server":"fixture","tool":"partial-wait","arguments":{}}))
            .unwrap();
    rt::block_on(async {
        let call = async {
            rig.registry
                .tools()
                .run("mcp_call", &input, None, &cx)
                .await
        };
        let cancel = async {
            let reactor = rt::Reactor::AsyncIo;
            while !rig.workspace().join("effects-cancel.ready").exists()
                || rig.progress.lock().unwrap().as_str() != "waiting"
            {
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

#[test]
fn fragmented_and_adjacent_messages_preserve_exact_results() {
    for tool in ["fragmented", "adjacent"] {
        let rig = rig(tool, Duration::from_secs(10));
        let text = "é🦀".repeat(4000);
        let result = rig.done(
            "mcp_call",
            &json!({"server":"fixture","tool":tool,"arguments":{"text":text}}),
        );
        assert_eq!(result["ok"]["content"][0]["text"], text);
        if tool == "adjacent" {
            assert_eq!(*rig.progress.lock().unwrap(), "first\nlast\n");
        }
        finish(rig);
    }
}

#[test]
fn concurrent_requests_start_once_and_reordered_replies_keep_their_owner() {
    let rig = std::sync::Arc::new(rig("multiplexed", Duration::from_secs(10)));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(128));
    let workers: Vec<_> = (0..128)
        .map(|index| {
            let rig = std::sync::Arc::clone(&rig);
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let result = rig.done(
                    "mcp_call",
                    &json!({"server":"fixture","tool":"reorder","arguments":{"index":index}}),
                );
                assert_eq!(result["ok"]["index"], index, "{result}");
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let effects = std::fs::read_to_string(rig.workspace().join("effects-multiplexed")).unwrap();
    assert_eq!(
        effects.lines().filter(|l| l.starts_with("start ")).count(),
        1
    );
    assert_eq!(effects.lines().filter(|l| *l == "discover").count(), 1);
    finish(std::sync::Arc::try_unwrap(rig).ok().unwrap());
}

struct DiscoveryPeer {
    url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DiscoveryPeer {
    fn new(mode: &'static str) -> Self {
        use std::io::Write;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut lists = 0;
            for stream in listener.incoming() {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let request = Self::read_request(&mut stream);
                recorded.lock().unwrap().push(request.clone());
                let is_list = request["method"] == "tools/list";
                let mut result = if is_list {
                    lists += 1;
                    let page = request["params"]["cursor"].as_str().unwrap_or("first");
                    let mut value = json!({"resultType":"complete","cacheScope":"private","ttlMs":300_000,"tools":[{"name":if page == "first" {"echo"} else {"second"},"description":format!("list {lists}"),"inputSchema":{"type":"object"}}]});
                    match mode {
                        "missing" | "cold-missing" => {
                            value.as_object_mut().unwrap().remove("ttlMs");
                        }
                        "zero" | "cold-zero" => value["ttlMs"] = json!(0),
                        "expired" => value["ttlMs"] = json!(1),
                        "pages" | "cold-pages" => {
                            if page == "first" {
                                value["ttlMs"] = json!(1);
                                value["nextCursor"] = json!("second");
                            } else {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                        }
                        "cursor-once" | "cursor-always" => {
                            if page == "first" {
                                value["tools"][0]["name"] = json!(if lists == 1 {
                                    "discarded"
                                } else {
                                    "fresh-first"
                                });
                                value["nextCursor"] = json!("second");
                            } else {
                                value["tools"][0]["name"] = json!("fresh-second");
                            }
                        }
                        "mrtr" if lists == 1 => {
                            value = json!({"resultType":"input_required","ttlMs":300_000,"cacheScope":"private","inputRequests":{}});
                        }
                        _ => {}
                    }
                    // Concurrent cold readers must overlap a real pending HTTP request.
                    if mode == "single-flight" || mode.starts_with("cold-") {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    value
                } else {
                    json!({"resultType":"complete","content":[]})
                };
                if !is_list {
                    result["tool"] = request["params"]["name"].clone();
                }
                let response = if request["params"]["cursor"].is_string()
                    && (mode == "cursor-always" || (mode == "cursor-once" && lists == 2))
                {
                    json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32602,"message":"invalid cursor"}})
                } else {
                    json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                };
                let (content_type, body) = if (mode == "notification" && !is_list)
                    || (mode == "notification-during-list" && is_list)
                {
                    let notice =
                        json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"});
                    (
                        "text/event-stream",
                        format!("data: {notice}\n\ndata: {response}\n\n").into_bytes(),
                    )
                } else {
                    ("application/json", serde_json::to_vec(&response).unwrap())
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Value {
        use std::io::Read;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let length = std::str::from_utf8(&head)
            .unwrap()
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn rig(&self) -> Rig {
        let mcp = Mcp::new(
            vec![Server {
                name: "fixture".parse().unwrap(),
                description: String::new(),
                transport: Transport::Http(self.url.clone()),
                env: BTreeMap::default(),
            }],
            Duration::from_secs(10),
        )
        .unwrap();
        Rig::new("discovery-cache", Registry::default().with(&mcp).unwrap())
    }

    fn lists(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "tools/list")
            .count()
    }
}
impl Drop for DiscoveryPeer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let address = self
            .url
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/mcp")
            .unwrap();
        let _ = std::net::TcpStream::connect(address);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn only_fresh_complete_lists_are_reused_and_every_page_limits_freshness() {
    for mode in [
        "positive",
        "missing",
        "zero",
        "expired",
        "pages",
        "mrtr",
        "notification-during-list",
    ] {
        let peer = DiscoveryPeer::new(mode);
        let rig = peer.rig();
        let first = rig.done("mcp_list", &json!({"server":"fixture"}));
        if mode == "mrtr" {
            assert!(first["failed"].get("protocol").is_some(), "{first}");
        } else {
            assert!(first.get("ok").is_some(), "{mode}: {first}");
        }
        if mode == "expired" {
            std::thread::sleep(Duration::from_millis(10));
        }
        let second = rig.done("mcp_list", &json!({"server":"fixture"}));
        assert!(second.get("ok").is_some(), "{mode}: {second}");
        assert_eq!(
            peer.lists(),
            match mode {
                "positive" => 1,
                "pages" => 4,
                _ => 2,
            },
            "{mode}"
        );
        if mode == "pages" {
            let requests = peer.requests.lock().unwrap();
            assert!(requests[0]["params"].get("cursor").is_none());
            assert_eq!(requests[1]["params"]["cursor"], "second");
        }
    }
}

#[test]
fn concurrent_cache_demand_sends_one_list_and_notifications_invalidate_it() {
    let peer = DiscoveryPeer::new("single-flight");
    let rig = std::sync::Arc::new(peer.rig());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(32));
    let workers: Vec<_> = (0..32)
        .map(|_| {
            let rig = std::sync::Arc::clone(&rig);
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                assert!(
                    rig.done("mcp_list", &json!({"server":"fixture"}))
                        .get("ok")
                        .is_some()
                );
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(peer.lists(), 1);
    let peer = DiscoveryPeer::new("notification");
    let rig = peer.rig();
    assert!(
        rig.done("mcp_list", &json!({"server":"fixture"}))
            .get("ok")
            .is_some()
    );
    assert!(
        rig.done(
            "mcp_call",
            &json!({"server":"fixture","tool":"echo","arguments":{}})
        )
        .get("ok")
        .is_some()
    );
    assert!(
        rig.done("mcp_list", &json!({"server":"fixture"}))
            .get("ok")
            .is_some()
    );
    assert_eq!(peer.lists(), 2);
}

#[test]
fn changing_a_sessions_launch_environment_selects_a_distinct_instance() {
    let rig = rig("exact-environment", Duration::from_secs(10));
    for identity in ["first", "second", "first"] {
        let mut cx = rig.cx();
        let mut env = protocol::Environment::new(
            cx.workspace.to_path_buf().try_into().unwrap(),
            std::env::vars_os(),
            false,
        );
        env.vars
            .retain(|key, _| !protocol::Environment::is_secret(key));
        env.vars.insert("A_MCP_IDENTITY".into(), identity.into());
        env.vars.insert("A_MCP_CALLER".into(), identity.into());
        cx.env = Some(std::sync::Arc::new(env));
        let input = protocol::Json::encode(
            &json!({"server":"fixture","tool":"launch-context","arguments":{}}),
        )
        .unwrap();
        let called = rt::block_on(rig.registry.tools().run("mcp_call", &input, None, &cx));
        let protocol::Outcome::Done {
            output: protocol::Payload::Inline(output),
        } = called.outcome
        else {
            panic!("no completed request")
        };
        let result: Value = output.parse().unwrap();
        assert_eq!(result["ok"]["identity"], identity);
        assert_eq!(result["ok"]["caller"], identity);
    }
    let effects =
        std::fs::read_to_string(rig.workspace().join("effects-exact-environment")).unwrap();
    assert_eq!(
        effects.lines().filter(|l| l.starts_with("start ")).count(),
        2
    );
    finish(rig);
}

#[test]
fn overlapping_cold_consumers_never_reuse_an_uncacheable_list() {
    for mode in ["cold-missing", "cold-zero", "cold-pages"] {
        let peer = DiscoveryPeer::new(mode);
        let rig = std::sync::Arc::new(peer.rig());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let rig = std::sync::Arc::clone(&rig);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let result = rig.done("mcp_list", &json!({"server":"fixture"}));
                    result["ok"][0]["description"].as_str().unwrap().to_owned()
                })
            })
            .collect();
        let snapshots: std::collections::BTreeSet<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            snapshots.len(),
            8,
            "{mode}: each consumer must receive its own changing response"
        );
        assert_eq!(
            peer.lists(),
            if mode == "cold-pages" { 16 } else { 8 },
            "{mode}"
        );
    }
}

#[test]
fn invalid_cursors_discard_partial_discovery_and_restart_with_a_bounded_budget() {
    let peer = DiscoveryPeer::new("cursor-once");
    let rig = peer.rig();
    let result = rig.done("mcp_list", &json!({"server":"fixture"}));
    assert_eq!(result["ok"][0]["name"], "fresh-first", "{result}");
    assert_eq!(result["ok"][1]["name"], "fresh-second");
    assert_eq!(peer.lists(), 4);
    assert_eq!(rig.done("mcp_list", &json!({"server":"fixture"})), result);
    assert_eq!(peer.lists(), 4);
    let peer = DiscoveryPeer::new("cursor-always");
    let rig = peer.rig();
    assert_eq!(
        rig.done("mcp_list", &json!({"server":"fixture"}))["failed"],
        "limit"
    );
    assert_eq!(peer.lists(), 64);
}

#[test]
fn stdio_cursor_restarts_never_publish_or_cache_the_discarded_pages() {
    let rig = rig("review-restart", Duration::from_secs(10));
    let result = rig.done("mcp_list", &json!({"server":"fixture"}));
    assert_eq!(result["ok"][0]["name"], "fresh-first", "{result}");
    assert_eq!(result["ok"][1]["name"], "fresh-second");
    assert_eq!(rig.done("mcp_list", &json!({"server":"fixture"})), result);
    let effects = std::fs::read_to_string(rig.workspace().join("effects-review-restart")).unwrap();
    assert_eq!(
        effects
            .lines()
            .filter(|line| line.starts_with("list "))
            .count(),
        4
    );
    finish(rig);
}

struct PolicySettings(sys::config::Table);
impl ext::Table for PolicySettings {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn text(&self, key: &str) -> Result<Option<&str>, ext::Refused> {
        self.0.text(key).map_err(|e| ext::Refused(e.to_string()))
    }
    fn whole(&self, key: &str) -> Result<Option<u64>, ext::Refused> {
        self.0.whole(key).map_err(|e| ext::Refused(e.to_string()))
    }
    fn flag(&self, key: &str) -> Result<Option<bool>, ext::Refused> {
        self.0.flag(key).map_err(|e| ext::Refused(e.to_string()))
    }
    fn list(&self, key: &str) -> Result<Option<&[String]>, ext::Refused> {
        self.0.list(key).map_err(|e| ext::Refused(e.to_string()))
    }
}

fn policy_mcp(scope: &str, argv: &[String], root: &std::path::Path) -> Mcp {
    use ext::Configured;
    let settings: sys::Config = format!(
        "[ext.mcp]\nservers=[\"fixture\"]\nfixture={}\nfixture_scope={scope:?}\nexperimental_stdio_sharing={}\n",
        serde_json::to_string(argv).unwrap(), scope != "session",
    ).parse().unwrap();
    Mcp::configure(&PolicySettings(settings.ext.table("mcp")))
        .unwrap()
        .declared_in(root)
        .unwrap()
}

#[derive(Clone)]
enum SpawnPolicy {
    Refuse,
    Wrap(Vec<String>),
}
impl Extension for SpawnPolicy {
    const NAME: &'static str = "spawn-policy";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut ext::Registrar) {
        let policy = self.clone();
        r.on::<ext::point::Spawn>(move |_, spawn| match &policy {
            Self::Refuse => ext::point::Rewrite::Stop("execution refused".into()),
            Self::Wrap(argv) => {
                let mut wrapped = spawn.clone();
                wrapped.argv.clone_from(argv);
                wrapped
                    .env
                    .insert("A_MCP_IDENTITY".into(), "wrapped".into());
                ext::point::Rewrite::Replace(wrapped)
            }
        });
    }
}

#[test]
fn session_spawn_policy_is_applied_and_shared_launches_reject_session_hooks() {
    for scope in ["session", "workspace", "user"] {
        for wrapping in [false, true] {
            let root = Rig::new("spawn-policy-root", Registry::default());
            let marker = root.dir.join("effects");
            let argv = vec![
                env!("CARGO_BIN_EXE_mcp-fixture").into(),
                marker.to_str().unwrap().into(),
            ];
            let policy = if wrapping {
                SpawnPolicy::Wrap(argv.clone())
            } else {
                SpawnPolicy::Refuse
            };
            let declaration = if wrapping {
                vec!["must-be-rewritten-by-spawn-policy".into()]
            } else {
                argv
            };
            let mcp = policy_mcp(scope, &declaration, &root.workspace());
            let rig = Rig::new(
                "spawn-policy",
                Registry::default()
                    .with(&ext_shell::Shell::default())
                    .unwrap()
                    .with(&mcp)
                    .unwrap()
                    .with(&policy)
                    .unwrap(),
            );
            let result = rig.done(
                "mcp_call",
                &json!({"server":"fixture","tool":"launch-context","arguments":{}}),
            );
            if scope != "session" {
                assert!(
                    result["failed"]["configuration"]
                        .as_str()
                        .is_some_and(|why| why.contains("spawn hooks")),
                    "{scope}: {result}"
                );
                assert!(!marker.exists());
            } else if wrapping {
                assert_eq!(result["ok"]["identity"], "wrapped", "{result}");
                assert!(marker.exists());
            } else {
                assert!(
                    result["failed"]["spawn"]
                        .as_str()
                        .is_some_and(|why| why.contains("execution refused")),
                    "{result}"
                );
                assert!(!marker.exists());
            }
            let child = marker.exists().then(|| {
                let effects = std::fs::read_to_string(&marker).unwrap();
                let pid = effects
                    .lines()
                    .next()
                    .unwrap()
                    .strip_prefix("start ")
                    .unwrap()
                    .parse()
                    .unwrap();
                let started = sys::process_started(pid).expect("the wrapped helper is live");
                (pid, started)
            });
            finish(rig);
            if let Some((pid, started)) = child {
                assert_ne!(sys::process_started(pid), Some(started));
            }
        }
    }
}

#[test]
fn a_hooked_session_cannot_reuse_an_already_admitted_shared_instance() {
    let root = Rig::new("shared-policy-root", Registry::default());
    let marker = root.dir.join("effects");
    let argv = vec![
        env!("CARGO_BIN_EXE_mcp-fixture").into(),
        marker.to_str().unwrap().into(),
    ];
    let mcp = policy_mcp("workspace", &argv, &root.workspace());
    let owner = Rig::new(
        "shared-policy-owner",
        Registry::default()
            .with(&ext_shell::Shell::default())
            .unwrap()
            .with(&mcp)
            .unwrap(),
    );
    let input = json!({"server":"fixture","tool":"launch-context","arguments":{}});
    assert!(owner.done("mcp_call", &input).get("ok").is_some());
    let hooked = Rig::new(
        "shared-policy-hooked",
        Registry::default()
            .with(&ext_shell::Shell::default())
            .unwrap()
            .with(&mcp)
            .unwrap()
            .with(&SpawnPolicy::Refuse)
            .unwrap(),
    );
    let mut cx = hooked.cx();
    cx.workspace = owner.cx().workspace;
    let result = rt::block_on(hooked.registry.tools().run(
        "mcp_call",
        &protocol::Json::encode(&input).unwrap(),
        None,
        &cx,
    ));
    let protocol::Outcome::Done {
        output: protocol::Payload::Inline(output),
    } = result.outcome
    else {
        panic!("{result:?}")
    };
    let output: Value = output.parse().unwrap();
    assert!(
        output["failed"]["configuration"]
            .as_str()
            .is_some_and(|why| why.contains("spawn hooks")),
        "{output}"
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("start "))
            .count(),
        1
    );
    assert!(owner.done("mcp_call", &input).get("ok").is_some());
    drop(cx);
    finish(hooked);
    finish(owner);
}

#[test]
fn form_and_url_rounds_preserve_server_keys_and_state_without_restarting_the_call() {
    use protocol::{Json, QuestionAnswer, Response, Seq};
    use std::collections::BTreeMap;
    let rig = rig("elicitation", Duration::from_secs(2));
    let cx = rig.cx();
    let input = Json::encode(&json!({"server":"fixture","tool":"ask","arguments":{}})).unwrap();
    let tools = rig.registry.tools();
    let tool = tools.get("mcp_call").unwrap();
    let called = rt::block_on(tools.run("mcp_call", &input, None, &cx));
    let first = called.parked.unwrap();
    let values = json!({"name":"Ada","age":37,"opt":false,"color":"red","tags":["one"]})
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), Json::encode(v).unwrap()))
        .collect();
    let answer = QuestionAnswer {
        question: Seq::new(1).unwrap(),
        responses: BTreeMap::from([("$continue".into(), Response::Submit(values))]),
    };
    first.question.validate(&answer).unwrap();
    // Human waiting has no exchange timer or retained request future.
    std::thread::sleep(Duration::from_millis(2100));
    let called =
        rt::block_on(tool.answer(tool.parse(&input).unwrap(), first.checkpoint, answer, &cx));
    let second = called.parked.unwrap();
    assert!(matches!(
        second.question.prompts["browser"],
        protocol::Prompt::Url { .. }
    ));
    let answer = QuestionAnswer {
        question: Seq::new(2).unwrap(),
        responses: BTreeMap::from([("browser".into(), Response::accept())]),
    };
    let called =
        rt::block_on(tool.answer(tool.parse(&input).unwrap(), second.checkpoint, answer, &cx));
    assert!(called.parked.is_none());
    assert!(matches!(called.outcome, protocol::Outcome::Done { .. }));
    let rounds =
        std::fs::read_to_string(rig.workspace().join("effects-elicitation.rounds")).unwrap();
    let rounds: Vec<Value> = rounds
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rounds.len(), 3);
    assert_ne!(rounds[0]["id"], rounds[1]["id"]);
    assert_ne!(rounds[1]["id"], rounds[2]["id"]);
    assert!(rounds[2]["params"].get("requestState").is_none());
    finish(rig);
}

#[test]
fn a_saved_continuation_is_refused_before_contacting_a_changed_server_configuration() {
    let rig = rig("question-binding", Duration::from_secs(2));
    let cx = rig.cx();
    let input =
        protocol::Json::encode(&json!({"server":"fixture","tool":"ask","arguments":{}})).unwrap();
    let first = rt::block_on(rig.registry.tools().run("mcp_call", &input, None, &cx))
        .parked
        .unwrap();
    let changed = Mcp::new(
        vec![Server {
            name: "fixture".parse().unwrap(),
            description: "test server".into(),
            transport: Transport::Stdio(vec![
                env!("CARGO_BIN_EXE_mcp-fixture").into(),
                "effects-question-binding".into(),
            ]),
            env: BTreeMap::from([("DECLARED_CONFIG".into(), "changed".into())]),
        }],
        Duration::from_secs(2),
    )
    .unwrap();
    let registry = Registry::default().with(&changed).unwrap();
    let tool = registry.tools().get("mcp_call").unwrap();
    let answer = protocol::QuestionAnswer {
        question: protocol::Seq::new(1).unwrap(),
        responses: first
            .question
            .prompts
            .keys()
            .map(|key| (key.clone(), protocol::Response::Decline))
            .collect(),
    };
    let result =
        rt::block_on(tool.answer(tool.parse(&input).unwrap(), first.checkpoint, answer, &cx));
    let protocol::Outcome::Done {
        output: protocol::Payload::Inline(output),
    } = result.outcome
    else {
        panic!("expected typed configuration refusal")
    };
    let output: Value = output.parse().unwrap();
    assert!(output["failed"].get("configuration").is_some(), "{output}");
    let rounds =
        std::fs::read_to_string(rig.workspace().join("effects-question-binding.rounds")).unwrap();
    assert_eq!(rounds.lines().count(), 1);
    finish(rig);
}

#[test]
fn declining_or_cancelling_a_state_only_round_sends_no_invented_input_key() {
    for action in [
        protocol::ContinueAnswer::Decline,
        protocol::ContinueAnswer::Cancel,
    ] {
        let rig = rig("state-only", Duration::from_secs(2));
        let cx = rig.cx();
        let input =
            protocol::Json::encode(&json!({"server":"fixture","tool":"ask_state","arguments":{}}))
                .unwrap();
        let tools = rig.registry.tools();
        let tool = tools.get("mcp_call").unwrap();
        let first = rt::block_on(tools.run("mcp_call", &input, None, &cx))
            .parked
            .unwrap();
        let answer = protocol::QuestionAnswer {
            question: protocol::Seq::new(1).unwrap(),
            responses: first
                .question
                .prompts
                .keys()
                .map(|key| (key.clone(), protocol::Response::Continue(action.clone())))
                .collect(),
        };
        first.question.validate(&answer).unwrap();
        let result =
            rt::block_on(tool.answer(tool.parse(&input).unwrap(), first.checkpoint, answer, &cx));
        assert!(result.parked.is_none());
        let protocol::Outcome::Done {
            output: protocol::Payload::Inline(output),
        } = result.outcome
        else {
            panic!("typed cancellation");
        };
        assert_eq!(
            output.parse::<Value>().unwrap(),
            json!({"failed":"cancelled"})
        );
        let effects = std::fs::read_to_string(rig.workspace().join("effects-state-only")).unwrap();
        assert_eq!(
            effects
                .lines()
                .filter(|line| line.starts_with("call "))
                .count(),
            1
        );
        finish(rig);
    }
}

#[test]
fn an_empty_input_request_map_can_continue_without_opaque_server_state() {
    let rig = rig("empty-input-map", Duration::from_secs(2));
    let cx = rig.cx();
    let input =
        protocol::Json::encode(&json!({"server":"fixture","tool":"ask_empty","arguments":{}}))
            .unwrap();
    let tools = rig.registry.tools();
    let tool = tools.get("mcp_call").unwrap();
    let first = rt::block_on(tools.run("mcp_call", &input, None, &cx))
        .parked
        .expect("an empty request map is a valid manual continuation");
    let answer = protocol::QuestionAnswer {
        question: protocol::Seq::new(1).unwrap(),
        responses: first
            .question
            .prompts
            .keys()
            .map(|key| {
                (
                    key.clone(),
                    protocol::Response::Continue(protocol::ContinueAnswer::Proceed),
                )
            })
            .collect(),
    };
    first.question.validate(&answer).unwrap();
    let result =
        rt::block_on(tool.answer(tool.parse(&input).unwrap(), first.checkpoint, answer, &cx));
    assert!(result.parked.is_none());
    let protocol::Outcome::Done {
        output: protocol::Payload::Inline(output),
    } = result.outcome
    else {
        panic!("a completed continuation");
    };
    assert_eq!(
        output.parse::<Value>().unwrap()["ok"]["resultType"],
        "complete"
    );
    finish(rig);
}

#[test]
fn unrelated_inherited_environment_changes_do_not_invalidate_a_saved_continuation() {
    let rig = rig("question-environment", Duration::from_secs(2));
    let mut cx = rig.cx();
    let input =
        protocol::Json::encode(&json!({"server":"fixture","tool":"ask_empty","arguments":{}}))
            .unwrap();
    let tools = rig.registry.tools();
    let tool = tools.get("mcp_call").unwrap();
    let first = rt::block_on(tools.run("mcp_call", &input, None, &cx))
        .parked
        .unwrap();
    let mut env = protocol::Environment::new(
        cx.workspace.to_path_buf().try_into().unwrap(),
        std::env::vars_os(),
        false,
    );
    env.vars
        .retain(|key, _| !protocol::Environment::is_secret(key));
    env.vars
        .insert("UNRELATED_INHERITED_VALUE".into(), "changed".into());
    cx.env = Some(std::sync::Arc::new(env));
    let answer = protocol::QuestionAnswer {
        question: protocol::Seq::new(1).unwrap(),
        responses: first
            .question
            .prompts
            .keys()
            .map(|key| {
                (
                    key.clone(),
                    protocol::Response::Continue(protocol::ContinueAnswer::Proceed),
                )
            })
            .collect(),
    };
    let result =
        rt::block_on(tool.answer(tool.parse(&input).unwrap(), first.checkpoint, answer, &cx));
    let protocol::Outcome::Done {
        output: protocol::Payload::Inline(output),
    } = result.outcome
    else {
        panic!("a completed continuation");
    };
    assert_eq!(
        output.parse::<Value>().unwrap()["ok"]["resultType"],
        "complete"
    );
    finish(rig);
}
