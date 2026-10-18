use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;

use agent::{Builtin, Extension, Hook, Registrar, Registry, Tool};
use bytes::Bytes;
use llm::http::{BoxFuture, Http};
use llm::protocols::responses;
use protocol::{Decision, Permission};
use serde::Deserialize as _;
use serde_json::json;

use super::*;

fn entry(seq: u64, body: Body) -> Entry {
    Entry {
        seq,
        epoch: 0,
        body,
    }
}

fn bodies(entries: &[Entry]) -> Vec<&Body> {
    entries.iter().map(|e| &e.body).collect()
}

#[test]
fn finds_calls_without_outcomes() {
    let tool_call = |tool: &str| Body::ToolCall {
        tool: tool.into(),
        input: json!({}),
        id: None,
    };
    let entries = [
        entry(1, tool_call("sleep")),
        entry(
            2,
            Body::ToolResult {
                call: 1,
                outcome: Outcome::Interrupted,
            },
        ),
        entry(3, tool_call("run_command")),
        entry(
            4,
            Body::ToolProcess {
                call: 3,
                pgid: 42,
                boot: "boot".into(),
            },
        ),
        entry(5, Body::ModelCall { model: "m".into() }),
    ];
    let dangling = dangling_calls(&entries);
    assert_eq!(dangling.len(), 2);
    assert_eq!(dangling[0].seq, 3);
    assert_eq!(dangling[0].processes, [(42, "boot".to_owned())]);
    assert!(matches!(dangling[1].call, Call::Model));
}

#[test]
fn transcripts_of_earlier_logs_read_replies_as_text() {
    let done = Outcome::Done {
        output: json!({ "text": "hello", "usage": { "input": 1, "output": 1 } }),
    };
    let entries = [
        entry(1, Body::User { text: "hi".into() }),
        entry(2, Body::ModelCall { model: "m".into() }),
        entry(
            3,
            Body::ModelResult {
                call: 2,
                outcome: done,
            },
        ),
        entry(
            4,
            Body::User {
                text: "again".into(),
            },
        ),
        entry(5, Body::ModelCall { model: "m".into() }),
    ];
    let turns = transcript(&entries, 5, &agent::Toolset::default());
    assert_eq!(
        turns,
        [
            llm::Message::user("hi"),
            llm::Message::assistant("hello"),
            llm::Message::user("again"),
        ]
    );
}

/// The tool rig's recordings call.
struct HarborLabel;

impl Tool for HarborLabel {
    type In = serde_json::Map<String, Value>;
    type Out = String;

    const NAME: &'static str = "lookup_harbor_label";
    const DESCRIPTION: &'static str = "Return the alpha signal marker.";
    const REPLAY: Replay = Replay::Safe;

    fn schema() -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    fn call(&self, _: Self::In, _: &Cx<'_>) -> impl Future<Output = Result<String, String>> + Send {
        std::future::ready(Ok("crimson-harbor".to_owned()))
    }

    fn to_model(label: &String) -> llm::Output {
        llm::Output::Text(label.clone())
    }
}

/// Offers only [`HarborLabel`].
struct Harbor;

impl Extension for Harbor {
    const NAME: &'static str = "harbor";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(HarborLabel);
    }
}

/// A model endpoint that answers each request with the next canned SSE
/// body, and keeps what it was sent.
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<String>>,
    sent: Mutex<Vec<Value>>,
}

impl Script {
    fn new(replies: &[String]) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.iter().cloned().collect()),
            ..Self::default()
        })
    }

    fn sent(&self) -> Vec<Value> {
        lock(&self.sent).clone()
    }
}

impl Http for Script {
    fn send(
        &self,
        request: http::Request<Bytes>,
    ) -> BoxFuture<'static, io::Result<llm::http::Response>> {
        lock(&self.sent).push(serde_json::from_slice(request.body()).unwrap_or_default());
        let reply = lock(&self.replies).pop_front().map(|body| {
            http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(llm::http::body([Bytes::from(body)]))
                .expect("scripted response")
        });
        Box::pin(std::future::ready(reply.ok_or_else(|| {
            io::Error::other("the script has no more replies")
        })))
    }
}

/// The replies recorded in a rig cassette, in order.
fn cassette(path: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/rig/cassettes")
        .join(path);
    let text = std::fs::read_to_string(path).unwrap();
    serde_yaml_ng::Deserializer::from_str(&text)
        .map(|doc| {
            let doc = serde_yaml_ng::Value::deserialize(doc).unwrap();
            let body = doc.get("then").and_then(|then| then.get("body"));
            body.and_then(serde_yaml_ng::Value::as_str)
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn openai(script: &Arc<Script>) -> Model {
    let (host, quirks) = responses::HOSTS
        .iter()
        .find(|(host, _)| host.id == "openai")
        .unwrap();
    let provider = llm::Provider::new(
        host.id,
        "http://scripted",
        llm::Auth::None,
        responses::Responses::new(*quirks),
    );
    Model::new(provider, "gpt-5.6".into(), Arc::clone(script))
}

struct Fixture(Arc<Shared>);

impl Fixture {
    fn new(agent: agent::Kind, registry: Registry, model: Model) -> Self {
        Self(Arc::new(Shared {
            // One worker: no effect may hold it.
            rt: Arc::new(rt::Threads::new(1)),
            store: Arc::new(store::Memory::default()),
            agent,
            registry,
            model: Ok(model),
            daemon: Weak::new(),
        }))
    }

    /// A session whose log holds `entries`, started as after a crash, once
    /// it has recovered.
    fn start(&self, entries: &[Entry]) -> Arc<Session> {
        let session = self.load(entries);
        settle(&session);
        session
    }

    /// [`Fixture::start`], without waiting for recovery.
    fn load(&self, entries: &[Entry]) -> Arc<Session> {
        let header = Header {
            id: SessionId::new(),
            workspace: std::env::temp_dir(),
            parent: None,
            extensions: installed(&self.0.registry),
        };
        let mut log = self.0.store.create(&header).unwrap().log;
        for e in entries {
            log.append(e).unwrap();
        }
        let loaded = self.0.store.load(header.id).unwrap();
        Session::start(header, loaded, Arc::clone(&self.0))
    }
}

/// A log that keeps the size of each batch it is handed, and fails while
/// told to.
#[derive(Clone, Default)]
struct Probe {
    batches: Arc<Mutex<Vec<usize>>>,
    failing: Arc<AtomicBool>,
}

impl Log for Probe {
    fn append(&mut self, entry: &Entry) -> io::Result<()> {
        self.append_all(std::slice::from_ref(entry))
    }

    fn append_all(&mut self, entries: &[Entry]) -> io::Result<()> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(io::Error::other("disk full"));
        }
        lock(&self.batches).push(entries.len());
        Ok(())
    }
}

#[test]
fn a_step_commits_its_entries_together_and_a_failed_one_leaves_no_trace() {
    let fixture = Fixture::new(
        agent::Kind::Echo,
        Registry::default(),
        openai(&Script::new(&[])),
    );
    let probe = Probe::default();
    let header = Header {
        id: SessionId::new(),
        workspace: std::env::temp_dir(),
        parent: None,
        extensions: Vec::new(),
    };
    let loaded = Loaded {
        entries: Vec::new(),
        log: Box::new(probe.clone()),
    };
    let session = Session::start(header, loaded, Arc::clone(&fixture.0));
    settle(&session);
    session.submit("hi".into());
    settle(&session);
    probe.failing.store(true, Ordering::SeqCst);
    session.submit("lost".into());
    settle(&session);
    probe.failing.store(false, Ordering::SeqCst);
    session.submit("again".into());
    settle(&session);
    let log = log_of(&session);
    session.stop();

    // Each message and its echo commit together; the failed pair is gone,
    // seqs and all.
    assert_eq!(*lock(&probe.batches), [2, 2]);
    let said: Vec<_> = log.iter().map(|e| (e.seq, &e.body)).collect();
    let user = |text: &str| Body::User { text: text.into() };
    let echo = |text: &str| Body::Assistant { text: text.into() };
    assert_eq!(
        said,
        [
            (1, &user("hi")),
            (2, &echo("hi")),
            (3, &user("again")),
            (4, &echo("again"))
        ]
    );
}

fn settle(session: &Session) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.has_work() {
        assert!(Instant::now() < deadline, "work never finished");
        thread::sleep(Duration::from_millis(5));
    }
}

fn log_of(session: &Session) -> Vec<Entry> {
    session.lock().entries.clone()
}

#[test]
fn the_model_calls_a_tool_and_answers_from_its_result() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies);
    let tools = Registry::default().with(&Harbor);
    let fixture = Fixture::new(agent::Kind::Chat, tools, openai(&script));
    let session = fixture.start(&[]);
    session.submit("Call the tool, then answer.".into());
    settle(&session);
    let log = log_of(&session);
    session.stop();
    let sent = script.sent();

    let call_id = "call_lWZlGyrTej1Dov0dGJDf1U0M";
    assert!(
        matches!(
            &bodies(&log)[..],
            [
                Body::User { .. },
                Body::ModelCall { .. },
                Body::ModelResult { call: 2, outcome: Outcome::Done { .. } },
                Body::ToolCall { tool, id: Some(id), .. },
                Body::ToolResult { call: 4, outcome: Outcome::Done { output } },
                Body::ModelCall { .. },
                Body::ModelResult { call: 6, outcome: Outcome::Done { output: answer } },
            ] if tool == "lookup_harbor_label" && id == call_id && output == "crimson-harbor"
                && answer["message"]["content"][1]["text"] == "The harbor label is crimson-harbor."
        ),
        "{log:#?}"
    );
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["tools"][0]["name"], "lookup_harbor_label");
    let items = sent[1]["input"].as_array().unwrap();
    let result = items
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(result["call_id"], call_id);
    assert_eq!(result["output"], "crimson-harbor");
    assert!(
        items
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == call_id),
        "the reply's call goes back before its result: {items:#?}"
    );

    // Stopped after any fact, a session picks up from its log and ends the
    // same way, sending the model the same second request.
    for cut in 3..=6 {
        let script = Script::new(&replies[1..]);
        let fixture = Fixture::new(
            agent::Kind::Chat,
            Registry::default().with(&Harbor),
            openai(&script),
        );
        let resumed = fixture.start(&log[..cut]);
        let got = log_of(&resumed);
        resumed.stop();
        assert_eq!(got, log, "cut after {cut}");
        assert_eq!(script.sent(), sent[1..], "cut after {cut}");
    }
}

#[test]
fn a_fired_watch_wakes_the_model_with_the_childs_result() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let fixture =
        |script: &Arc<Script>| Fixture::new(agent::Kind::Chat, Registry::default(), openai(script));
    let child = SessionId::new();
    let script = Script::new(&replies[1..]);
    let session = fixture(&script).load(&[entry(1, Body::Watch { child })]);
    let found = Outcome::Done {
        output: "found it".into(),
    };
    session.settle(child, found.clone());
    settle(&session);
    let log = log_of(&session);
    session.stop();
    let sent = script.sent();

    assert!(
        matches!(
            &bodies(&log)[1..],
            [
                Body::Fired { watch: 1, outcome },
                Body::ModelCall { .. },
                Body::ModelResult { call: 3, outcome: Outcome::Done { .. } },
            ] if *outcome == found
        ),
        "{log:#?}"
    );
    let input = sent[0]["input"].to_string();
    assert!(
        input.contains("The child session finished:\\nfound it"),
        "{input}"
    );

    // Stopped after the watch fired, recovery takes the same step.
    let script = Script::new(&replies[1..]);
    let resumed = fixture(&script).start(&log[..2]);
    let got = log_of(&resumed);
    resumed.stop();
    assert_eq!(got, log);
    assert_eq!(script.sent(), sent);
}

#[test]
fn extensions_are_logged_when_they_change() {
    let script = Script::new(&[]);
    let registry = Registry::default().with(&Harbor).with(&Builtin);
    let fixture = Fixture::new(agent::Kind::Echo, registry, openai(&script));
    // Created with these extensions: nothing to log.
    let same = fixture.start(&[]);
    let unchanged = log_of(&same);
    same.stop();
    // An older set, as an upgrade leaves it, is superseded once.
    let older = Body::Extensions {
        installed: vec![Installed {
            name: "harbor".into(),
            version: 0,
        }],
    };
    let upgraded = fixture.start(&[entry(1, older)]);
    let noted = log_of(&upgraded);
    upgraded.stop();

    assert_eq!(unchanged, []);
    assert_eq!(
        bodies(&noted)[1..],
        [&Body::Extensions {
            installed: installed(&fixture.0.registry)
        }]
    );
}

/// Denies shell commands, and has the agent go on once after it stops.
struct Policy;

impl Extension for Policy {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on(Policy);
    }
}

impl Hook for Policy {
    fn permission(&self, tool: &str, _: &Value) -> Permission {
        if tool == agent::tools::RunCommand::NAME {
            Permission::Deny
        } else {
            Permission::Allow
        }
    }

    fn on_stop(&self, entries: &[Entry]) -> Option<String> {
        let continued = entries.iter().any(|e| {
            matches!(
                e.body,
                Body::Hook {
                    decision: Decision::OnStop { .. },
                    ..
                }
            )
        });
        (!continued).then(|| "and again".to_owned())
    }
}

#[test]
fn hook_decisions_are_logged_and_replay_to_the_same_state() {
    let fixture = Fixture::new(
        agent::Kind::Echo,
        Registry::default().with(&Builtin).with(&Policy),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    session.submit("$ echo hi".into());
    settle(&session);
    let log = log_of(&session);
    session.stop();

    let policy = |decision| Body::Hook {
        hook: "policy".into(),
        decision,
    };
    assert_eq!(
        bodies(&log),
        [
            &Body::User {
                text: "$ echo hi".into()
            },
            &policy(Decision::Permission {
                tool: "run_command".into(),
                permission: Permission::Deny,
            }),
            &Body::ToolCall {
                tool: "run_command".into(),
                input: json!({ "command": "echo hi" }),
                id: None,
            },
            &Body::ToolResult {
                call: 3,
                outcome: Outcome::Failed {
                    error: "policy denied this call".into()
                },
            },
            &policy(Decision::OnStop {
                text: "and again".into()
            }),
            &Body::Assistant {
                text: "and again".into()
            },
        ]
    );
    assert!(replay(agent::Kind::Echo.agent().as_mut(), &log).is_empty());
    assert_eq!(
        transcript(&log, u64::MAX, &agent::Toolset::default()),
        [
            llm::Message::user("$ echo hi"),
            llm::Message::user("and again"),
            llm::Message::assistant("and again")
        ]
    );
}

/// A crash after a gate's decisions but before its call: recovery gates the
/// call again without logging the decisions twice.
#[test]
fn a_gate_cut_off_by_a_crash_logs_its_decisions_once() {
    let fixture = Fixture::new(
        agent::Kind::Echo,
        Registry::default().with(&Builtin).with(&Policy),
        openai(&Script::new(&[])),
    );
    let denied = Body::Hook {
        hook: "policy".into(),
        decision: Decision::Permission {
            tool: "run_command".into(),
            permission: Permission::Deny,
        },
    };
    let user = Body::User {
        text: "$ echo hi".into(),
    };
    let session = fixture.start(&[entry(1, user), entry(2, denied.clone())]);
    let log = log_of(&session);
    session.stop();

    let decisions = log.iter().filter(|e| e.body == denied).count();
    assert_eq!(decisions, 1, "{log:#?}");
    assert!(matches!(log[2].body, Body::ToolCall { .. }), "{log:#?}");
}

/// One worker runs every session: a wait for approval, a sleep and running
/// commands all overlap.
#[test]
fn sessions_wait_on_effects_without_holding_the_worker() {
    let ask = agent::AskFor(vec![agent::tools::Sleep::NAME.to_owned()]);
    let fixture = Fixture::new(
        agent::Kind::Echo,
        Registry::default().with(&Builtin).with(&ask),
        openai(&Script::new(&[])),
    );
    let asked = fixture.start(&[]);
    asked.submit("sleep 1".into());
    settle(&asked);
    let started = Instant::now();
    let commands: Vec<_> = (0..2)
        .map(|_| {
            let session = fixture.start(&[]);
            session.submit("$ sleep 1".into());
            session
        })
        .collect();
    asked.answer(3, true).unwrap();
    for session in &commands {
        finished(session, 2);
        session.stop();
    }
    finished(&asked, 3);
    let took = started.elapsed();
    asked.stop();
    assert!(
        took < Duration::from_millis(1900),
        "ran one at a time: {took:?}"
    );
}

fn asking() -> Fixture {
    let ask = agent::AskFor(vec![agent::tools::RunCommand::NAME.to_owned()]);
    Fixture::new(
        agent::Kind::Echo,
        Registry::default().with(&Builtin).with(&ask),
        openai(&Script::new(&[])),
    )
}

/// Waits for call `call`'s outcome to be committed.
fn finished(session: &Session, call: u64) -> Vec<Entry> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = log_of(session);
        if log
            .iter()
            .any(|e| matches!(e.body, Body::ToolResult { call: c, .. } if c == call))
        {
            return log;
        }
        assert!(Instant::now() < deadline, "call {call} never finished");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn an_ask_waits_in_the_inbox_through_a_stop_and_runs_once_approved() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo approved".into());
    // Waiting for an answer is not work.
    settle(&session);
    let asked = log_of(&session);
    assert_eq!(inbox(&asked), [3]);
    assert!(
        session.answer(2, true).is_err(),
        "only asked calls take answers"
    );
    session.stop();

    let resumed = fixture.start(&asked);
    let (tx, rx) = mpsc::channel();
    resumed.attach(&tx, asked.len() as u64);
    resumed.answer(3, true).unwrap();
    let log = finished(&resumed, 3);
    resumed.stop();

    let sent: Vec<DaemonMsg> = rx.try_iter().collect();
    assert!(
        matches!(&sent[..], [
            DaemonMsg::Attached { .. },
            DaemonMsg::Synced { seq: 4 },
            DaemonMsg::Ask { call: 3, tool, .. },
            ..
        ] if tool == "run_command"),
        "{sent:#?}"
    );
    assert!(
        matches!(
            &bodies(&log)[..],
            [
                Body::User { .. },
                Body::Hook {
                    decision: Decision::Permission {
                        permission: Permission::Ask,
                        ..
                    },
                    ..
                },
                Body::ToolCall { .. },
                Body::Ask { call: 3 },
                Body::Hook {
                    decision: Decision::Approval {
                        approved: true,
                        call: Some(3),
                        ..
                    },
                    ..
                },
                Body::ToolProcess { call: 3, .. },
                Body::ToolResult {
                    call: 3,
                    outcome: Outcome::Done { output },
                },
            ] if output["stdout"] == "approved\n"
        ),
        "{log:#?}"
    );
    assert_eq!(inbox(&log), Vec::<u64>::new());
}

#[test]
fn a_refused_ask_answers_the_call_without_running_it() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo refused".into());
    settle(&session);
    session.answer(3, false).unwrap();
    let log = finished(&session, 3);
    session.stop();
    assert_eq!(
        bodies(&log)[4..],
        [
            &Body::Hook {
                hook: "user".into(),
                decision: Decision::Approval {
                    tool: "run_command".into(),
                    approved: false,
                    call: Some(3),
                },
            },
            &Body::ToolResult {
                call: 3,
                outcome: Outcome::Failed {
                    error: "the user did not approve this call".into()
                },
            },
        ]
    );
}

#[test]
fn a_recap_tells_the_last_prompt_and_what_followed() {
    let call = |tool: &str| Body::ToolCall {
        tool: tool.into(),
        input: json!({}),
        id: None,
    };
    let entries = [
        entry(
            1,
            Body::User {
                text: "first".into(),
            },
        ),
        entry(
            2,
            Body::Assistant {
                text: "first".into(),
            },
        ),
        entry(
            3,
            Body::User {
                text: "go on\nwith detail".into(),
            },
        ),
        entry(4, call("sleep")),
        entry(
            5,
            Body::ToolResult {
                call: 4,
                outcome: Outcome::Interrupted,
            },
        ),
        entry(6, call("run_command")),
        entry(7, Body::Ask { call: 6 }),
        entry(8, call("sleep")),
    ];
    assert_eq!(recap(&entries, 8), None);
    assert_eq!(
        recap(&entries, 0),
        Some(protocol::Recap {
            prompt: Some("go on…".into()),
            tools: vec!["sleep".into(), "run_command".into(), "sleep".into()],
            failed: 1,
            running: 1,
            reply: None,
            asks: 1,
        })
    );
    let replied = recap(&entries[..2], 1).unwrap();
    assert_eq!(replied.reply.as_deref(), Some("first"));
    let env = entry(1, Body::Environment(Environment::default()));
    assert_eq!(
        recap(&[env], 0),
        None,
        "an environment alone is nothing to tell"
    );
}
