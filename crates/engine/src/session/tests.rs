use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use bytes::Bytes;
use ext::host::Registry;
use ext::{Extension, Registrar, Rewrite, Tool, point};
use ext_policy::Policy;
use llm::http::{BoxFuture, Http};
use protocol::{Caps, Card, Decision, Permission};
use serde::Deserialize as _;
use serde_json::json;

use super::*;

fn entry(seq: u64, body: Body) -> Entry {
    Entry::new(seq, 0, body)
}

fn bodies(entries: &[Entry]) -> Vec<&Body> {
    entries.iter().map(|e| &e.body).collect()
}

/// The calls asked about and not yet answered.
fn inbox(entries: &[Entry]) -> Vec<u64> {
    Ledger::of(entries).inbox().to_vec()
}

/// Whether input waits to be taken up.
fn untaken(entries: &[Entry]) -> Vec<ledger::Queued> {
    Ledger::of(entries).untaken().iter().cloned().collect()
}

/// What an attach after `since` is told of `entries`.
fn recap_of(entries: &[Entry], since: u64) -> Option<protocol::Recap> {
    let ledger = Ledger::of(entries);
    let after = entries.partition_point(|e| e.seq <= ledger.turn_start());
    recap(&ledger, facts::reply(&entries[after..]).as_deref(), since)
}

/// The conversation before `before`, as `registry`'s tools present it.
fn transcript(entries: &[Entry], before: u64, registry: Arc<Registry>) -> Vec<llm::Message> {
    let before = entries.iter().take_while(|e| e.seq < before);
    Transcript::of(before, registry, "")
        .into_messages()
        .to_vec()
}

/// What entries say, whichever lease wrote them.
fn facts(entries: &[Entry]) -> Vec<(u64, &Body)> {
    entries.iter().map(|e| (e.seq, &e.body)).collect()
}

#[test]
fn finds_calls_without_outcomes() {
    let tool_call = |tool: &str| Body::ToolCall {
        tool: tool.into(),
        input: json!({}).into(),
        id: None,
        card: None,
        by: Audience::Model,
    };
    let entries = [
        entry(1, tool_call("sleep")),
        entry(
            2,
            Body::ToolResult {
                call: 1,
                outcome: Outcome::Interrupted { output: None },
                card: None,
            },
        ),
        entry(3, tool_call("bash")),
        entry(
            4,
            Body::ToolProcess {
                call: 3,
                pgid: 42,
                boot: "boot".into(),
                background: false,
            },
        ),
        entry(
            5,
            Body::ModelCall {
                model: "m".into(),
                attempt: 0,
            },
        ),
    ];
    let dangling = dangling_calls(&Ledger::of(&entries));
    assert_eq!(dangling.len(), 2);
    assert_eq!(dangling[0].seq, 3);
    assert_eq!(dangling[0].processes, [(42, "boot".to_owned())]);
    assert!(matches!(dangling[1].call, Call::Model));
}

#[test]
fn transcripts_of_earlier_logs_read_replies_as_text() {
    let done = Outcome::Done {
        output: json!({ "text": "hello", "usage": { "input": 1, "output": 1 } }).into(),
    };
    let entries = [
        entry(1, Body::User { text: "hi".into() }),
        entry(
            2,
            Body::ModelCall {
                model: "m".into(),
                attempt: 0,
            },
        ),
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
        entry(
            5,
            Body::ModelCall {
                model: "m".into(),
                attempt: 0,
            },
        ),
    ];
    let turns = transcript(&entries, 5, Arc::default());
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

    fn call(&self, _: Self::In, _: &Cx) -> impl Future<Output = Result<String, String>> + Send {
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
/// body, and keeps what it was sent and the credential it came with.
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<String>>,
    sent: Mutex<Vec<Value>>,
    keys: Mutex<Vec<String>>,
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

    fn keys(&self) -> Vec<String> {
        lock(&self.keys).clone()
    }
}

impl Http for Script {
    fn send(
        &self,
        request: http::Request<llm::http::Payload>,
    ) -> BoxFuture<'static, io::Result<llm::http::Response>> {
        let body = request.body().to_vec();
        lock(&self.sent).push(serde_json::from_slice(&body).unwrap_or_default());
        let key = request.headers().get("authorization");
        lock(&self.keys).push(key.and_then(|k| k.to_str().ok()).unwrap_or("").to_owned());
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

/// The home config file of these tests: `gpt-5.6` on `OpenAI`, with a key.
/// Each call writes a file of its own: rewriting one shared file truncates
/// it under a session that is reading it.
fn gpt_home() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("a-engine-gpt-{}-{n}.toml", std::process::id()));
    std::fs::write(
        &path,
        "model = \"gpt-5.6\"\n[env]\nOPENAI_API_KEY = \"daemon\"\n",
    )
    .unwrap();
    path
}

/// `gpt-5.6` on `OpenAI`, answered by `script`.
fn openai(script: &Arc<Script>) -> Models {
    with_home(script, &gpt_home())
}

/// [`openai`], with the home config file at `path`.
fn with_home(script: &Arc<Script>, path: &Path) -> Models {
    Models {
        settings: crate::config::Settings::new(path.to_owned(), None),
        http: Arc::clone(script) as Arc<dyn Http>,
    }
}

struct Fixture(Arc<Shared>);

/// The agents sessions under test run.
struct Agents;

impl Extension for Agents {
    const NAME: &'static str = "agents";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.agent(ext_chat::CHAT, || Box::new(ext_chat::Loop::default()))
            .agent(ext_fixtures::ECHO, || Box::new(ext_fixtures::Echo));
    }
}

impl Fixture {
    fn new(agent: &str, registry: Registry, models: Models) -> Self {
        Self::on(Arc::new(store::Memory::default()), agent, registry, models)
    }

    fn on(store: Arc<dyn LogStore>, agent: &str, registry: Registry, mut models: Models) -> Self {
        models.settings.agent = Some(agent.to_owned());
        // The agent, for a registry under test that has none.
        let registry = if registry.agents().any(|a| a == agent) {
            registry
        } else {
            registry.with_in(ext::Tier::Post, &Agents).unwrap()
        };
        Self(Arc::new(Shared {
            // One worker: no effect may hold it.
            rt: Arc::new(rt::Threads::new(1)),
            store,
            registry: ext::host::Current::new(registry),
            models,
            system: None,
            spill: std::env::temp_dir().join(format!("a-spill-{}", std::process::id())),
            loaded: Mutex::default(),
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
            extensions: installed(&self.0.registry.get()),
        };
        let mut created = self.0.store.create(&header).unwrap();
        for e in entries {
            let e = Entry::new(e.seq, created.epoch, e.body.clone());
            created.log.append(&e).unwrap();
        }
        Session::start(header, Arc::clone(&self.0), Open::Load).unwrap()
    }
}

/// A store whose logs keep the size of each batch they are handed, and
/// fail while told to.
#[derive(Clone, Default)]
struct Probe {
    store: Arc<store::Memory>,
    batches: Arc<Mutex<Vec<usize>>>,
    failing: Arc<AtomicBool>,
}

struct Probed(Box<dyn Log>, Probe);

impl Log for Probed {
    fn append_all(&mut self, entries: &[Entry]) -> io::Result<()> {
        if self.1.failing.load(Ordering::SeqCst) {
            return Err(io::Error::other("disk full"));
        }
        lock(&self.1.batches).push(entries.len());
        self.0.append_all(entries)
    }
}

impl LogStore for Probe {
    fn create(&self, header: &Header) -> io::Result<Loaded> {
        let mut loaded = self.store.create(header)?;
        loaded.log = Box::new(Probed(loaded.log, self.clone()));
        Ok(loaded)
    }

    fn headers(&self) -> io::Result<Vec<Header>> {
        self.store.headers()
    }

    fn load(&self, id: SessionId, each: &mut dyn FnMut(Entry)) -> io::Result<Loaded> {
        self.store.load(id, each)
    }

    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>> {
        self.store.entries(id)
    }

    fn count(&self, id: SessionId) -> io::Result<u64> {
        self.store.count(id)
    }

    fn mark_busy(&self, id: SessionId, busy: bool) -> io::Result<()> {
        self.store.mark_busy(id, busy)
    }

    fn is_busy(&self, id: SessionId) -> bool {
        self.store.is_busy(id)
    }
}

#[test]
fn input_commits_on_arrival_and_a_failed_commit_leaves_no_trace() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        Registry::default(),
        openai(&Script::new(&[])),
    );
    let header = Header {
        id: SessionId::new(),
        workspace: std::env::temp_dir(),
        parent: None,
        extensions: installed(&fixture.0.registry.get()),
    };
    let session = Session::start(header, Arc::clone(&fixture.0), Open::Create).unwrap();
    settle(&session);
    session.submit("hi".into()).unwrap();
    settle(&session);
    probe.failing.store(true, Ordering::SeqCst);
    let lost = session.submit("lost".into());
    settle(&session);
    probe.failing.store(false, Ordering::SeqCst);
    session.submit("again".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();

    // Input commits as it arrives, before the turn it starts. Input that
    // cannot be logged is refused and gone, seq and all.
    assert_eq!(lost, Err("logging the input: disk full".into()));
    assert_eq!(*lock(&probe.batches), [1, 1, 1, 1]);
    let said: Vec<_> = log.iter().map(|e| (e.seq, &e.body)).collect();
    assert!(
        matches!(
            &said[..],
            [
                (1, Body::User { text: hi }),
                (2, Body::Assistant { .. }),
                (3, Body::User { text: again }),
                (4, Body::Assistant { .. }),
            ] if hi == "hi" && again == "again"
        ),
        "{said:#?}"
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
    session.frames.entries().collect()
}

#[test]
fn the_model_calls_a_tool_and_answers_from_its_result() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies);
    let tools = Registry::default().with(&Harbor).unwrap();
    let fixture = Fixture::new(ext_chat::CHAT, tools, openai(&script));
    let session = fixture.start(&[]);
    session
        .submit("Call the tool, then answer.".into())
        .unwrap();
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
                Body::ToolResult { call: 4, outcome: Outcome::Done { output }, card: None },
                Body::ModelCall { .. },
                Body::ModelResult { call: 6, outcome: Outcome::Done { output: answer } },
            ] if tool == "lookup_harbor_label" && id == call_id && output.value() == "crimson-harbor"
                && answer.value()["message"]["content"][1]["text"] == "The harbor label is crimson-harbor."
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
            ext_chat::CHAT,
            Registry::default().with(&Harbor).unwrap(),
            openai(&script),
        );
        let resumed = fixture.start(&log[..cut]);
        let got = log_of(&resumed);
        resumed.stop();
        assert_eq!(facts(&got), facts(&log), "cut after {cut}");
        assert_eq!(script.sent(), sent[1..], "cut after {cut}");
    }
}

#[test]
fn model_calls_take_credentials_from_the_client_then_the_config() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let dir = std::env::temp_dir().join(format!("a-engine-credentials-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    let key_sent = |client: Option<&str>, file: Option<&str>| {
        let text = file.map_or_else(String::new, |key| {
            format!("model = \"gpt-5.6\"\n[env]\nOPENAI_API_KEY = \"{key}\"\n")
        });
        std::fs::write(&config, text).unwrap();
        let script = Script::new(&replies);
        let fixture = Fixture::new(
            ext_chat::CHAT,
            Registry::default().with(&Harbor).unwrap(),
            with_home(&script, &config),
        );
        let session = fixture.start(&[]);
        if let Some(key) = client {
            let vars = [("OPENAI_API_KEY".into(), key.into())];
            session
                .set_env(Environment::new(dir.clone(), vars, false))
                .unwrap();
        }
        session
            .submit("Call the tool, then answer.".into())
            .unwrap();
        settle(&session);
        session.stop();
        let keys = script.keys();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], keys[1]);
        keys[0].clone()
    };
    assert_eq!(key_sent(Some("client"), Some("file")), "Bearer client");
    assert_eq!(key_sent(None, Some("file")), "Bearer file");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_missing_credential_fails_the_model_call_saying_where_to_set_it() {
    let script = Script::new(&[]);
    let config = std::env::temp_dir().join("a-engine-no-config.toml");
    let models = with_home(&script, &config);
    let fixture = Fixture::new(ext_chat::CHAT, Registry::default(), models);
    let session = fixture.start(&[]);
    session.submit("hi".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();
    let error = match &bodies(&log)[..] {
        [
            Body::User { .. },
            Body::ModelCall { model, .. },
            Body::ModelResult {
                outcome: Outcome::Failed { error },
                ..
            },
            ..,
        ] if model == "none" => error.clone(),
        other => panic!("{other:#?}"),
    };
    assert!(error.contains("OPENAI_API_KEY"), "{error}");
    assert!(error.contains(&config.display().to_string()), "{error}");
    assert_eq!(script.sent(), Vec::<Value>::new());
}

/// The engine's model path against a real endpoint: `mise exec -- cargo
/// nextest run -p engine --run-ignored only live`. Keys, base URL and
/// `A_MODEL` come from this test's environment, as a client's would.
#[test]
#[ignore = "calls a real endpoint"]
fn live_the_model_calls_a_tool_and_answers_from_its_result() {
    let rt: rt::Rt = Arc::new(rt::Threads::new(2));
    let home = std::env::temp_dir().join(format!("a-engine-live-{}.toml", std::process::id()));
    let mut file = sys::Config {
        model: std::env::var("A_MODEL").ok(),
        ..sys::Config::default()
    };
    for name in ["OPENAI_API_KEY", "OPENAI_BASE_URL"] {
        if let Ok(value) = std::env::var(name) {
            file.env.insert(name.to_owned(), value);
        }
    }
    file.write(&home).unwrap();
    let models = Models {
        settings: crate::config::Settings::new(home, None),
        http: Arc::new(llm::Ureq::new(rt)),
    };
    let fixture = Fixture::new(
        ext_chat::CHAT,
        Registry::default().with(&Harbor).unwrap(),
        models,
    );
    let session = fixture.start(&[]);
    session
        .submit("Call lookup_harbor_label, then tell me the label it returned.".into())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while session.has_work() {
        assert!(Instant::now() < deadline, "no answer");
        thread::sleep(Duration::from_millis(50));
    }
    let log = log_of(&session);
    session.stop();
    assert!(
        log.iter().any(|e| matches!(
            &e.body,
            Body::ToolResult { outcome: Outcome::Done { output }, .. } if output.value() == "crimson-harbor"
        )),
        "{log:#?}"
    );
    let Some(Body::ModelResult {
        outcome: Outcome::Done { output },
        ..
    }) = log.last().map(|e| &e.body)
    else {
        panic!("{log:#?}");
    };
    assert!(output.to_string().contains("crimson-harbor"), "{output}");
}

#[test]
fn a_fired_watch_wakes_the_model_with_the_childs_result() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let fixture =
        |script: &Arc<Script>| Fixture::new(ext_chat::CHAT, Registry::default(), openai(script));
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
    assert_eq!(facts(&got), facts(&log));
    assert_eq!(script.sent(), sent);
}

#[test]
fn extensions_are_logged_when_they_change() {
    let script = Script::new(&[]);
    let registry = Registry::default()
        .with(&Harbor)
        .and_then(|r| r.with(&ext_shell::Shell))
        .unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&script));
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
            installed: installed(&fixture.0.registry.get())
        }]
    );
}

/// Every extension and the fixtures, with `policy` as given.
fn echoing(policy: &Policy) -> Registry {
    crate::tests::extensions(policy).unwrap()
}

/// In place of the `policy` extension: denies shell commands, and has the
/// agent go on once after it stops.
struct Strict;

impl Extension for Strict {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<point::Permission>(|_, call| {
            if call.tool == ext_shell::Bash::NAME {
                Permission::Deny
            } else {
                Permission::Allow
            }
        })
        .on::<point::OnStop>(|_, history| {
            let continued = history.entries().any(|e| {
                matches!(
                    e.body,
                    Body::Hook {
                        decision: Decision::OnStop { .. },
                        ..
                    }
                )
            });
            (!continued).then(|| "and again".to_owned())
        });
    }
}

#[test]
fn hook_decisions_are_logged_and_replay_to_the_same_state() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default())
            .with_in(ext::Tier::Guarded, &Strict)
            .unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    session.submit("$ echo hi".into()).unwrap();
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
                tool: "bash".into(),
                permission: Permission::Deny,
            }),
            &Body::ToolCall {
                tool: "bash".into(),
                input: json!({ "command": "echo hi" }).into(),
                id: None,
                card: Some(Card {
                    title: "$ echo hi".into(),
                    ..Card::default()
                }),
                by: Audience::Model,
            },
            &Body::ToolResult {
                call: 3,
                outcome: Outcome::Failed {
                    error: "policy denied this call".into()
                },
                card: None,
            },
            &policy(Decision::OnStop {
                text: "and again".into()
            }),
            &Body::Assistant {
                text: "and again".into()
            },
        ]
    );
    assert!(replay(&mut ext_fixtures::Echo, &log).is_empty());
    assert_eq!(
        transcript(&log, u64::MAX, Arc::default()),
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
        ext_fixtures::ECHO,
        echoing(&Policy::default())
            .with_in(ext::Tier::Guarded, &Strict)
            .unwrap(),
        openai(&Script::new(&[])),
    );
    let denied = Body::Hook {
        hook: "policy".into(),
        decision: Decision::Permission {
            tool: "bash".into(),
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
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::ask(&["sleep"])),
        openai(&Script::new(&[])),
    );
    let asked = fixture.start(&[]);
    asked.submit("sleep 1".into()).unwrap();
    settle(&asked);
    let started = Instant::now();
    let commands: Vec<_> = (0..2)
        .map(|_| {
            let session = fixture.start(&[]);
            session.submit("$ sleep 1".into()).unwrap();
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
    Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::ask(&["bash"])),
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
    session.submit("$ echo approved".into()).unwrap();
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
    let outbox = Outbox::new(Caps::NONE);
    resumed.attach(&outbox, asked.len() as u64);
    resumed.answer(3, true).unwrap();
    let log = finished(&resumed, 3);
    resumed.stop();

    let sent: Vec<DaemonMsg> = std::iter::from_fn(|| outbox.try_next())
        .map(|frame: Frame| {
            protocol::read_frame(&mut frame.as_bytes())
                .unwrap()
                .unwrap()
        })
        .collect();
    assert!(
        matches!(&sent[..], [
            DaemonMsg::Attached { .. },
            DaemonMsg::Synced { seq: 4 },
            DaemonMsg::Ask { call: 3, tool, .. },
            ..
        ] if tool == "bash"),
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
                    card: Some(card),
                },
            ] if output.value()["output"] == "approved\n" && card.note == "exit 0"
        ),
        "{log:#?}"
    );
    assert_eq!(inbox(&log), Vec::<u64>::new());
}

#[test]
fn a_refused_ask_answers_the_call_without_running_it() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo refused".into()).unwrap();
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
                    tool: "bash".into(),
                    approved: false,
                    call: Some(3),
                },
            },
            &Body::ToolResult {
                call: 3,
                outcome: Outcome::Failed {
                    error: "the user did not approve this call".into()
                },
                card: None,
            },
        ]
    );
}

/// An interrupt settles a call waiting for approval: approving it after
/// runs nothing.
#[test]
fn an_interrupt_while_asked_settles_the_call_without_running_it() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo ran".into()).unwrap();
    settle(&session);
    session.interrupt();
    // Too late: it may be settled already.
    let _ = session.answer(3, true);
    finished(&session, 3);
    settle(&session);
    let log = log_of(&session);
    session.stop();
    assert!(
        !log.iter()
            .any(|e| matches!(e.body, Body::ToolProcess { .. })),
        "{log:#?}"
    );
    assert!(
        log.iter().any(|e| e.body
            == Body::ToolResult {
                call: 3,
                outcome: Outcome::Interrupted { output: None },
                card: None,
            }),
        "{log:#?}"
    );
    assert_eq!(inbox(&log), Vec::<u64>::new());
}

/// Allows every call, interrupting the session as it gates one: an
/// interrupt that lands while the gate's decisions commit.
#[derive(Clone, Default)]
struct Interrupter(Arc<std::sync::OnceLock<Weak<Session>>>);

impl Extension for Interrupter {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let this = self.clone();
        r.on::<point::Permission>(move |_, _| {
            if let Some(session) = this.0.get().and_then(Weak::upgrade) {
                session.interrupt();
            }
            Permission::Allow
        });
    }
}

#[test]
fn an_interrupt_while_a_call_is_gated_starts_nothing() {
    let interrupter = Interrupter::default();
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default())
            .with_in(ext::Tier::Guarded, &interrupter)
            .unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    interrupter.0.set(Arc::downgrade(&session)).unwrap();
    session.submit("$ echo ran".into()).unwrap();
    finished(&session, 2);
    settle(&session);
    let log = log_of(&session);
    session.stop();
    assert!(
        !log.iter()
            .any(|e| matches!(e.body, Body::ToolProcess { .. })),
        "{log:#?}"
    );
    assert!(
        log.iter().any(|e| e.body
            == Body::ToolResult {
                call: 2,
                outcome: Outcome::Interrupted { output: None },
                card: None,
            }),
        "{log:#?}"
    );
}

/// A client sees a call a crash left running as soon as the session loads:
/// an interrupt before recovery starts stops it, rather than going to work
/// that already ended.
#[test]
fn an_interrupt_before_recovery_starts_stops_it() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    // Holds the one worker, so the actor has not started yet.
    let (release, held) = std::sync::mpsc::channel::<()>();
    fixture.0.rt.spawn(Box::pin(async move {
        let _ = held.recv();
    }));
    let session = fixture.load(&[entry(
        1,
        Body::ToolCall {
            tool: "sleep".into(),
            input: json!({ "seconds": 1 }).into(),
            id: None,
            card: None,
            by: Audience::Model,
        },
    )]);
    session.interrupt();
    drop(release);
    let log = finished(&session, 1);
    settle(&session);
    session.stop();
    assert_eq!(
        log[1].body,
        Body::ToolResult {
            call: 1,
            outcome: Outcome::Interrupted { output: None },
            card: None,
        },
        "{log:#?}"
    );
}

/// An interrupt with nothing running stops nothing: not the next turn.
#[test]
fn an_interrupt_between_turns_is_not_kept_for_the_next() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.interrupt();
    session.submit("hi".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();
    assert!(
        matches!(
            &bodies(&log)[..],
            [Body::User { .. }, Body::Assistant { .. }]
        ),
        "{log:#?}"
    );
}

#[test]
fn a_recap_tells_the_last_prompt_and_what_followed() {
    let call = |tool: &str| Body::ToolCall {
        tool: tool.into(),
        input: json!({}).into(),
        id: None,
        card: None,
        by: Audience::Model,
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
                outcome: Outcome::Interrupted { output: None },
                card: None,
            },
        ),
        entry(6, call("bash")),
        entry(7, Body::Ask { call: 6 }),
        entry(8, call("sleep")),
    ];
    assert_eq!(recap_of(&entries, 8), None);
    assert_eq!(
        recap_of(&entries, 0),
        Some(protocol::Recap {
            prompt: Some("go on…".into()),
            tools: vec!["sleep".into(), "bash".into(), "sleep".into()],
            failed: 1,
            running: 1,
            reply: None,
            asks: 1,
        })
    );
    let replied = recap_of(&entries[..2], 1).unwrap();
    assert_eq!(replied.reply.as_deref(), Some("first"));
    let env = entry(1, Body::Environment(Environment::default()));
    assert_eq!(
        recap_of(&[env], 0),
        None,
        "an environment alone is nothing to tell"
    );
}

/// Waits until `done` holds for the session's log.
fn until(session: &Session, what: &str, done: impl Fn(&[Entry]) -> bool) -> Vec<Entry> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = log_of(session);
        if done(&log) {
            return log;
        }
        assert!(Instant::now() < deadline, "never {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// `/jobs` answers without a model turn; `shout` rewrites input and
/// refuses `no`; every `bash` call goes to the background; failed model
/// requests are retried as `retried`.
struct Extras;

struct Jobs;

impl ext::Command for Jobs {
    const NAME: &'static str = "jobs";
    const DESCRIPTION: &'static str = "Lists jobs.";

    fn run(&self, args: &str, cx: &Cx) -> impl Future<Output = Result<Value, String>> + Send {
        let said = format!("{args} in {}", cx.call);
        std::future::ready(Ok(said.into()))
    }
}

impl Extension for Extras {
    const NAME: &'static str = "extras";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.command(Jobs)
            .on::<point::Input>(|_, text| match text {
                "no" => Rewrite::Stop("not that".into()),
                text if text.starts_with("shout ") => Rewrite::Replace(text.to_uppercase()),
                _ => Rewrite::Pass,
            })
            .on::<point::PreTool>(|_, call| {
                if call.tool != "bash" || call.input["background"] == true {
                    return Rewrite::Pass;
                }
                let mut input = call.input.clone();
                input["background"] = true.into();
                Rewrite::Replace(input)
            })
            .on::<point::RequestError>(|_, failed| {
                Some(llm::Request {
                    model: "retried".into(),
                    ..failed.request.clone()
                })
            });
    }
}

#[test]
fn slash_commands_run_without_a_turn_and_input_hooks_rewrite_or_refuse() {
    let registry = Registry::default().with(&Extras).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("/jobs all of them".into()).unwrap();
    session.submit("/nope".into()).unwrap();
    session.submit("shout hi".into()).unwrap();
    assert_eq!(session.submit("no".into()), Err("extras: not that".into()));
    settle(&session);
    let log = log_of(&session);
    session.stop();

    // The command's result commits as soon as it has run: before or after
    // the input behind it is taken in, but before that input is taken up.
    let (ran, rest): (Vec<&Entry>, Vec<&Entry>) = log
        .iter()
        .partition(|e| matches!(e.body, Body::CommandResult { .. }));
    let done = Body::CommandResult {
        call: 1,
        outcome: Outcome::Done {
            output: "all of them in 1".into(),
        },
    };
    assert!(matches!(&ran[..], [e] if e.seq < rest[4].seq && e.body == done));
    let rest: Vec<Entry> = (1..)
        .zip(rest)
        .map(|(seq, e)| entry(seq, e.body.clone()))
        .collect();
    assert_eq!(
        bodies(&rest),
        [
            &Body::Command {
                name: "jobs".into(),
                args: "all of them".into()
            },
            &queued("/nope"),
            &Body::Hook {
                hook: "extras".into(),
                decision: Decision::Input {
                    original: "shout hi".into()
                }
            },
            &queued("SHOUT HI"),
            &user("/nope"),
            &echo("/nope"),
            &user("SHOUT HI"),
            &echo("SHOUT HI"),
        ]
    );
    assert!(replay(&mut ext_fixtures::Echo, &log).is_empty());
    assert_eq!(untaken(&log), []);

    // A command cut off by a crash is not run again; the input queued
    // behind it is taken up in order.
    let cut = fixture.start(&rest[..4]);
    let recovered = log_of(&cut);
    cut.stop();
    assert_eq!(
        bodies(&recovered)[4..],
        [
            &Body::CommandResult {
                call: 1,
                outcome: Outcome::Interrupted { output: None }
            },
            &user("/nope"),
            &echo("/nope"),
            &user("SHOUT HI"),
            &echo("SHOUT HI"),
        ]
    );
}

fn user(text: &str) -> Body {
    Body::User { text: text.into() }
}

fn echo(text: &str) -> Body {
    Body::Assistant { text: text.into() }
}

fn queued(text: &str) -> Body {
    Body::Queued {
        text: text.into(),
        steer: false,
    }
}

#[test]
fn the_model_gets_the_prompt_and_the_logged_context_first() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies[1..]);
    let fixture = Fixture::new(ext_chat::CHAT, echoing(&Policy::default()), openai(&script));
    let session = fixture.start(&[]);
    session.submit("hi".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();

    let Body::Context { text } = &log[1].body else {
        panic!("{log:#?}")
    };
    assert!(
        text.starts_with("<environment>\nworking directory: "),
        "{text}"
    );
    assert!(matches!(log[2].body, Body::ModelCall { .. }));
    let sent = script.sent();
    let instructions = sent[0]["instructions"].as_str().unwrap();
    assert!(instructions.starts_with("You are `a`"), "{instructions}");
    assert_eq!(sent[0]["input"][0]["content"][0]["text"], *text);
    assert_eq!(sent[0]["input"][1]["content"][0]["text"], "hi");
    let tools: Vec<_> = sent[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["bash", "edit", "sleep"]);
    assert_eq!(
        transcript(&log, u64::MAX, fixture.0.registry.get())[0],
        llm::Message::user(text.clone())
    );
}

#[test]
fn a_failed_request_is_retried_as_a_hook_asks() {
    let registry = Registry::default().with(&Extras).unwrap();
    let script = Script::new(&[]);
    let fixture = Fixture::new(ext_chat::CHAT, registry, openai(&script));
    let session = fixture.start(&[]);
    session.submit("hi".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();

    let retries: Vec<_> = log
        .iter()
        .filter_map(|e| match &e.body {
            Body::Hook {
                decision: Decision::Retry { call, attempt, .. },
                ..
            } => Some((*call, *attempt)),
            _ => None,
        })
        .collect();
    assert_eq!(retries, [(2, 1), (2, 2)]);
    let models: Vec<_> = script.sent().iter().map(|r| r["model"].clone()).collect();
    assert_eq!(models, ["gpt-5.6", "retried", "retried"]);
    assert!(matches!(
        &log.last().unwrap().body,
        Body::ModelResult {
            outcome: Outcome::Failed { .. },
            ..
        }
    ));
}

#[test]
fn the_shell_directory_survives_a_restart_and_an_interrupt_stops_a_call() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    let dir = format!("a-cwd-{}", std::process::id());
    session
        .submit(format!("$ mkdir -p {dir} && cd {dir}"))
        .unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();

    let again = fixture.start(&log);
    again.submit("$ pwd".into()).unwrap();
    let ran = finished(&again, log.len() as u64 + 2);
    let Some(Body::ToolResult {
        outcome: Outcome::Done { output },
        ..
    }) = ran.last().map(|e| &e.body)
    else {
        panic!("{ran:#?}")
    };
    assert!(
        output.value()["output"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/{dir}\n")),
        "{output}"
    );

    again.submit("$ sleep 100".into()).unwrap();
    until(&again, "started", |log| {
        log.iter()
            .filter(|e| matches!(e.body, Body::ToolProcess { .. }))
            .count()
            == 3
    });
    again.interrupt();
    settle(&again);
    let log = log_of(&again);
    again.stop();
    let _ = std::fs::remove_dir(std::env::temp_dir().join(&dir));
    let Some(Body::ToolResult {
        outcome: Outcome::Done { output },
        ..
    }) = log.last().map(|e| &e.body)
    else {
        panic!("{log:#?}")
    };
    assert_eq!(output.value()["aborted"], true, "{output}");
}

#[test]
fn background_processes_keep_the_session_and_end_with_it() {
    let registry = echoing(&Policy::default()).with(&Extras).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("$ sleep 100".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    let pgid = log
        .iter()
        .find_map(|e| match e.body {
            Body::ToolProcess {
                pgid,
                background: true,
                ..
            } => Some(pgid),
            _ => None,
        })
        .unwrap();
    assert!(session.has_jobs());
    assert_eq!(session.idle_for(), Duration::ZERO);
    session.stop();
    let deadline = Instant::now() + Duration::from_secs(5);
    while sys::group_alive(pgid) {
        assert!(Instant::now() < deadline, "the job outlived its session");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!session.has_jobs());
}

/// `lookup_harbor_label`, held until released or the turn is interrupted.
struct Held(Arc<AtomicBool>);

impl Tool for Held {
    type In = serde_json::Map<String, Value>;
    type Out = String;

    const NAME: &'static str = "lookup_harbor_label";
    const DESCRIPTION: &'static str = "Return the alpha signal marker.";

    fn schema() -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn call(&self, _: Self::In, cx: &Cx) -> Result<String, String> {
        while !self.0.load(Ordering::SeqCst) {
            if cx.cancel.is_cancelled() {
                return Err("stopped".into());
            }
            cx.rt.sleep(Duration::from_millis(5)).await;
        }
        Ok("crimson-harbor".into())
    }
}

/// Offers [`Held`], released by the flag it is given.
struct HeldHarbor(Arc<AtomicBool>);

impl Extension for HeldHarbor {
    const NAME: &'static str = "harbor";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Held(Arc::clone(&self.0)));
    }
}

/// A model endpoint that never answers.
struct Silent;

impl Http for Silent {
    fn send(
        &self,
        _: http::Request<llm::http::Payload>,
    ) -> BoxFuture<'static, io::Result<llm::http::Response>> {
        Box::pin(std::future::pending())
    }
}

fn tool_running(log: &[Entry]) -> bool {
    log.iter().any(|e| matches!(e.body, Body::ToolCall { .. }))
}

#[test]
fn a_steer_reaches_the_running_turn_before_the_model_is_next_asked() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies);
    let release = Arc::new(AtomicBool::new(false));
    let tools = Registry::default()
        .with(&HeldHarbor(Arc::clone(&release)))
        .unwrap();
    let fixture = Fixture::new(ext_chat::CHAT, tools, openai(&script));
    let session = fixture.start(&[]);
    session
        .submit("Call the tool, then answer.".into())
        .unwrap();
    until(&session, "called the tool", tool_running);
    session.steer("Say it twice.".into()).unwrap();
    session.submit("And then this.".into()).unwrap();
    release.store(true, Ordering::SeqCst);
    let log = until(&session, "took the queued input", |log| {
        log.iter()
            .filter(|e| matches!(e.body, Body::User { .. }))
            .count()
            == 2
    });
    session.stop();

    let after_tool: Vec<_> = bodies(&log)
        .into_iter()
        .skip_while(|b| !matches!(b, Body::ToolResult { .. }))
        .collect();
    assert!(
        matches!(
            &after_tool[..],
            [
                Body::ToolResult { .. },
                Body::Steer { text, queued: 5 },
                Body::ModelCall { .. },
                Body::ModelResult { .. },
                Body::User { text: then },
                ..
            ] if text == "Say it twice." && then == "And then this."
        ),
        "{log:#?}"
    );
    assert!(matches!(&log[4].body, Body::Queued { steer: true, .. }));
    let input = script.sent()[1]["input"].to_string();
    assert!(input.contains("Say it twice."), "{input}");
    assert_eq!(untaken(&log), []);
}

#[test]
fn an_interrupt_stops_the_model_call_and_ends_the_turn() {
    let models = Models {
        settings: crate::config::Settings::new(gpt_home(), None),
        http: Arc::new(Silent),
    };
    let fixture = Fixture::new(ext_chat::CHAT, Registry::default(), models);
    let session = fixture.start(&[]);
    session.submit("hi".into()).unwrap();
    until(&session, "called the model", |log| {
        log.iter().any(|e| matches!(e.body, Body::ModelCall { .. }))
    });
    session.interrupt();
    settle(&session);
    let log = log_of(&session);
    session.stop();
    assert_eq!(
        bodies(&log)[2..],
        [&Body::ModelResult {
            call: 2,
            outcome: Outcome::Interrupted { output: None }
        }]
    );
}

#[test]
fn an_interrupted_turn_answers_the_calls_it_leaves_and_goes_no_further() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies);
    let tools = Registry::default()
        .with(&HeldHarbor(Arc::default()))
        .unwrap();
    let fixture = Fixture::new(ext_chat::CHAT, tools, openai(&script));
    let session = fixture.start(&[]);
    session
        .submit("Call the tool, then answer.".into())
        .unwrap();
    until(&session, "called the tool", tool_running);
    session.interrupt();
    settle(&session);
    let log = log_of(&session);
    session.stop();

    assert!(
        matches!(
            &bodies(&log)[3..],
            [
                Body::ToolCall { .. },
                Body::ToolResult {
                    call: 4,
                    outcome: Outcome::Failed { .. },
                    ..
                },
                Body::Interrupt { .. },
            ]
        ),
        "{log:#?}"
    );
    assert_eq!(script.sent().len(), 1, "no model call after the interrupt");
    // Recovery finds nothing left to do.
    let mut chat: Box<dyn Agent> = Box::new(ext_chat::Loop::default());
    assert_eq!(replay(chat.as_mut(), &log), []);
}

#[test]
fn an_unsafe_call_cut_off_reports_the_output_it_committed() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let bash = Body::ToolCall {
        tool: "bash".into(),
        input: json!({ "command": "make" }).into(),
        id: None,
        card: None,
        by: Audience::Model,
    };
    let progress = |text: &str| Body::Progress {
        call: 2,
        text: text.into(),
    };
    let session = fixture.start(&[
        entry(1, user("$ make")),
        entry(2, bash),
        entry(3, progress("one\n")),
        entry(4, progress("two\n")),
    ]);
    let log = log_of(&session);
    session.stop();
    assert_eq!(
        bodies(&log)[4..],
        [&Body::ToolResult {
            call: 2,
            outcome: Outcome::Interrupted {
                output: Some("one\ntwo\n".into())
            },
            card: None,
        }]
    );
    let (output, error) = ext::host::Toolset::default().to_model(
        "bash",
        &Outcome::Interrupted {
            output: Some("one\n".into()),
        },
    );
    assert!(error);
    assert_eq!(
        output,
        llm::Output::Text("one\n\n[interrupted before it finished]".into())
    );
}

#[test]
fn the_model_is_set_per_session_and_says_where_it_comes_from() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default(),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    for text in [
        "/model",
        "/model anthropic:claude-x",
        "/model",
        "/model reset",
    ] {
        session.submit(text.into()).unwrap();
        settle(&session);
    }
    let log = log_of(&session);
    let results: Vec<Value> = log
        .iter()
        .filter_map(|e| match &e.body {
            Body::CommandResult {
                outcome: Outcome::Done { output },
                ..
            } => Some(output.value()),
            _ => None,
        })
        .collect();
    let set = |from: &str, model: &str| json!({ "model": model, "from": from });
    assert_eq!(
        results,
        [
            set("the home config.toml", "gpt-5.6"),
            set("this session, at entry 4", "anthropic:claude-x"),
            set("this session, at entry 4", "anthropic:claude-x"),
            set("the home config.toml", "gpt-5.6"),
        ]
    );
    let chosen = Body::Settings(Overrides {
        model: Some("anthropic:claude-x".into()),
    });
    assert_eq!(log[3].body, chosen);
    assert_eq!(session.model_spec(), "gpt-5.6");
    session.stop();

    // The session's own setting lasts in its log.
    let again = fixture.start(&log[..5]);
    assert_eq!(again.model_spec(), "anthropic:claude-x");
    again.stop();
}

#[test]
fn a_new_registry_generation_applies_from_the_next_step() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    session.submit("hi".into()).unwrap();
    settle(&session);
    let reloaded = echoing(&Policy::default()).with(&Extras).unwrap();
    fixture.0.registry.replace(reloaded);
    session.submit("/jobs now".into()).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.stop();
    let after: Vec<_> = bodies(&log).into_iter().skip(2).collect();
    assert!(
        matches!(
            &after[..],
            [
                Body::Command { .. },
                Body::Extensions { installed },
                Body::CommandResult { outcome: Outcome::Done { .. }, .. },
            ] if installed.last().is_some_and(|i| i.name == "extras")
        ),
        "{log:#?}"
    );
}

#[test]
fn input_waits_its_turn_behind_a_call_waiting_for_approval() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo approved".into()).unwrap();
    settle(&session);
    session.submit("next".into()).unwrap();
    let waiting = log_of(&session);
    session.answer(3, true).unwrap();
    let log = until(&session, "took the input", |log| {
        log.iter().any(|e| e.body == echo("next"))
    });
    session.stop();

    assert_eq!(waiting.last().map(|e| &e.body), Some(&queued("next")));
    let after: Vec<_> = bodies(&log)
        .into_iter()
        .skip_while(|b| !matches!(b, Body::ToolResult { .. }))
        .skip(1)
        .collect();
    assert_eq!(after, [&user("next"), &echo("next")]);
}

/// Input that arrives while a loaded session recovers is in the log as
/// queued when recovery looks for the input still queued there: it is
/// taken up once, after what was queued before the load.
#[test]
fn input_taken_in_during_recovery_runs_once() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default(),
        openai(&Script::new(&[])),
    );
    let before = [
        entry(1, user("one")),
        entry(2, echo("one")),
        entry(3, queued("left")),
    ];
    for _ in 0..20 {
        let session = fixture.load(&before);
        session.submit("two".into()).unwrap();
        settle(&session);
        let log = log_of(&session);
        session.stop();
        let said: Vec<_> = bodies(&log)
            .into_iter()
            .filter(|b| !matches!(b, Body::Queued { .. }))
            .collect();
        assert_eq!(
            said,
            [
                &user("one"),
                &echo("one"),
                &user("left"),
                &echo("left"),
                &user("two"),
                &echo("two"),
            ]
        );
    }
}
mod interrupt;
mod log;
