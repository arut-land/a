//! A live session against its effects: a scripted model, real tools and
//! stores, crashes cut at every fact.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Weak};
use std::thread;
use std::time::Duration;

use crate::store::{Loaded, Log, LogStore, Memory};
use bytes::Bytes;
use ext::{Extension, Registrar, Replay, Rewrite, Tool, point};
use ext_policy::Policy;
use llm::http::Http;
use protocol::{CallId, Caps, Card, Permission, Seq, WaitOn};
use serde::Deserialize as _;
use serde_json::{Value, json};

use super::*;

mod interrupt;
mod log;
mod retry;

fn entry(seq: u64, body: Body) -> Entry {
    Entry::new(seq, 0, body)
}

fn bodies(entries: &[Entry]) -> Vec<&Body> {
    entries.iter().map(|e| &e.body).collect()
}

fn id(seq: u64) -> CallId {
    CallId::new(seq).unwrap()
}

/// Input that started a turn as it arrived.
fn user(text: &str) -> Body {
    Body::Input {
        text: text.into(),
        mode: Mode::Queue,
        taken: true,
        origin: None,
    }
}

/// Input that waits to be taken up.
fn queued(text: &str) -> Body {
    Body::Input {
        text: text.into(),
        mode: Mode::Queue,
        taken: false,
        origin: None,
    }
}

fn echo(text: &str) -> Body {
    Body::Assistant { text: text.into() }
}

fn decision(by: &str, call: Option<u64>, d: Verdict) -> Body {
    Body::Decision {
        by: by.into(),
        call: call.map(id),
        d,
    }
}

/// The calls asked about and not yet answered.
fn inbox(entries: &[Entry]) -> Vec<u64> {
    protocol::ledger::Ledger::of(entries).inbox().to_vec()
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
    ) -> llm::http::BoxFuture<'static, io::Result<llm::http::Response>> {
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
        Self::on(Arc::new(Memory::default()), agent, registry, models)
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

    fn header(&self) -> Header {
        Header {
            id: SessionId::new(),
            workspace: std::env::temp_dir(),
            parent: None,
            extensions: installed(&self.0.registry.get()),
        }
    }

    /// A new session.
    fn create(&self) -> Arc<Live> {
        Live::start(&self.header(), &self.0, Open::Create, Owner::None).unwrap()
    }

    /// A session whose log holds `entries`, started as after a crash, once
    /// it has recovered.
    fn start(&self, entries: &[Entry]) -> Arc<Live> {
        let session = self.load(entries);
        settle(&session);
        session
    }

    /// [`Fixture::start`], without waiting for recovery.
    fn load(&self, entries: &[Entry]) -> Arc<Live> {
        let header = self.header();
        let mut created = self.0.store.create(&header).unwrap();
        for e in entries {
            let e = Entry::new(e.seq, created.writer.epoch(), e.body.clone());
            created
                .writer
                .log
                .append_all(std::slice::from_ref(&e))
                .unwrap();
        }
        drop(created);
        Live::start(&header, &self.0, Open::Load, Owner::None).unwrap()
    }
}

impl Live {
    fn steer(&self, text: &str) -> Result<(), String> {
        let text = text.to_owned();
        let steer = Request::Input {
            text,
            mode: Mode::Steer,
        };
        rt::block_on(self.request(steer, None))
    }

    fn answer(&self, call: u64, approved: bool) -> Result<(), String> {
        let answer = Request::Answer {
            call,
            value: approved.into(),
        };
        rt::block_on(self.request(answer, None))
    }

    fn interrupt_now(&self) {
        let _ = rt::block_on(self.request(Request::Interrupt, None));
    }

    /// Stops the actor and waits for it to end.
    fn halt(&self) {
        self.stop();
        rt::block_on(self.ended.over());
    }

    fn follow(&self, outbox: &Outbox, since: u64) {
        rt::block_on(self.attach(outbox.clone(), since));
    }

    fn model_spec(&self) -> String {
        self.session.model_spec()
    }
}

/// Waits until the session has taken what it was sent and has no work.
fn settle(session: &Live) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        session.barrier();
        if !session.has_work() {
            return;
        }
        assert!(Instant::now() < deadline, "work never finished");
        thread::sleep(Duration::from_millis(5));
    }
}

fn log_of(session: &Live) -> Vec<Entry> {
    session.frames.entries().collect()
}

/// Waits until `done` holds for the session's log.
fn until(session: &Live, what: &str, done: impl Fn(&[Entry]) -> bool) -> Vec<Entry> {
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

/// Waits for call `call`'s outcome to be committed.
fn finished(session: &Live, call: u64) -> Vec<Entry> {
    until(session, &format!("finished call {call}"), |log| {
        log.iter()
            .any(|e| matches!(e.body, Body::ToolResult { call: c, .. } if c == call))
    })
}

/// A store whose logs keep the size of each batch they are handed, and
/// fail while told to.
#[derive(Clone, Default)]
struct Probe {
    store: Arc<Memory>,
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
        loaded.writer.log = Box::new(Probed(loaded.writer.log, self.clone()));
        Ok(loaded)
    }

    fn headers(&self) -> io::Result<Vec<Header>> {
        self.store.headers()
    }

    fn load(&self, id: SessionId, each: &mut dyn FnMut(Entry)) -> io::Result<Loaded> {
        let mut loaded = self.store.load(id, each)?;
        loaded.writer.log = Box::new(Probed(loaded.writer.log, self.clone()));
        Ok(loaded)
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
fn transcripts_of_earlier_logs_read_replies_as_text() {
    let done = Outcome::Done {
        output: json!({ "text": "hello", "usage": { "input": 1, "output": 1 } }).into(),
    };
    let model_call = || Body::ModelCall {
        model: "m".into(),
        attempt: 0,
    };
    let entries = [
        entry(1, Body::User { text: "hi".into() }),
        entry(2, model_call()),
        entry(
            3,
            Body::ModelResult {
                call: 2,
                outcome: done,
            },
        ),
        entry(4, user("again")),
        entry(5, model_call()),
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

/// #9 `a_failed_commit_is_resume`: input commits on arrival, before the
/// turn it starts; input that cannot be logged is refused and gone, seq
/// and all, and the session goes on from its log.
#[test]
fn input_commits_on_arrival_and_a_failed_commit_leaves_no_trace() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        Registry::default(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session.submit("hi").unwrap();
    settle(&session);
    probe.failing.store(true, Ordering::SeqCst);
    let lost = session.submit("lost");
    settle(&session);
    probe.failing.store(false, Ordering::SeqCst);
    session.submit("again").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    assert_eq!(lost, Err("logging: disk full".into()));
    assert_eq!(*lock(&probe.batches), [1, 1, 1, 1]);
    assert_eq!(
        facts(&log),
        [
            (1, &user("hi")),
            (2, &echo("hi")),
            (3, &user("again")),
            (4, &echo("again"))
        ]
    );
}

#[test]
fn the_model_calls_a_tool_and_answers_from_its_result() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies);
    let tools = Registry::default().with(&Harbor).unwrap();
    let fixture = Fixture::new(ext_chat::CHAT, tools, openai(&script));
    let session = fixture.start(&[]);
    session.submit("Call the tool, then answer.").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    let sent = script.sent();

    let call_id = "call_lWZlGyrTej1Dov0dGJDf1U0M";
    assert!(
        matches!(
            &bodies(&log)[..],
            [
                Body::Input { .. },
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

    // #8 `recovery_is_resume`: stopped after any fact, a session picks up
    // from its log and ends the same way, sending the model the same second
    // request. A model call cut off is an attempt that failed: its retry is
    // logged.
    for cut in 3..=6 {
        let script = Script::new(&replies[1..]);
        let registry = Registry::default().with(&Harbor).unwrap();
        let retry = ext_retry::Retry {
            base: Duration::ZERO,
            ..ext_retry::Retry::default()
        };
        let fixture = Fixture::new(
            ext_chat::CHAT,
            registry.with(&retry).unwrap(),
            openai(&script),
        );
        let resumed = fixture.start(&log[..cut]);
        let got = log_of(&resumed);
        resumed.halt();
        let mut want = bodies(&log);
        let retried = decision(
            "retry",
            Some(6),
            Verdict::Retry {
                attempt: 1,
                error: "the daemon stopped during the call".into(),
            },
        );
        if cut == 6 {
            want.insert(6, &retried);
        }
        assert_eq!(bodies(&got), want, "cut after {cut}");
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
            session.set_env(Environment::new(dir.clone(), vars, false));
        }
        session.submit("Call the tool, then answer.").unwrap();
        settle(&session);
        session.halt();
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
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    let error = match &bodies(&log)[..] {
        [
            Body::Input { .. },
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
        .submit("Call lookup_harbor_label, then tell me the label it returned.")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while session.has_work() {
        assert!(Instant::now() < deadline, "no answer");
        thread::sleep(Duration::from_millis(50));
    }
    let log = log_of(&session);
    session.halt();
    assert!(
        log.iter().any(|e| matches!(
            &e.body,
            Body::ToolResult { outcome: Outcome::Done { output }, .. } if output.value() == "crimson-harbor"
        )),
        "{log:#?}"
    );
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
    session.settled(child, found.clone());
    settle(&session);
    let log = log_of(&session);
    session.halt();
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
    resumed.halt();
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
    same.halt();
    // An older set, as an upgrade leaves it, is superseded once.
    let older = Body::Extensions {
        installed: vec![Installed {
            name: "harbor".into(),
            version: 0,
        }],
    };
    let upgraded = fixture.start(&[entry(1, older)]);
    let noted = log_of(&upgraded);
    upgraded.halt();

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
                    Body::Decision {
                        d: Verdict::OnStop { .. },
                        ..
                    }
                )
            });
            (!continued).then(|| "and again".to_owned())
        });
    }
}

#[test]
fn hook_decisions_are_logged_after_their_call_and_replay_to_the_same_state() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default())
            .with_in(ext::Tier::Guarded, &Strict)
            .unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    session.submit("$ echo hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let denied = Verdict::Permission {
        permission: Permission::Deny,
    };
    assert_eq!(
        bodies(&log),
        [
            &user("$ echo hi"),
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
            &decision("policy", Some(2), denied),
            &Body::ToolResult {
                call: 2,
                outcome: Outcome::Failed {
                    error: "policy denied this call".into()
                },
                card: None,
            },
            &decision(
                "policy",
                None,
                Verdict::OnStop {
                    text: "and again".into()
                }
            ),
            &echo("and again"),
        ]
    );
    // Loaded again, it has nothing left to do.
    let again = fixture.start(&log);
    let reloaded = log_of(&again);
    again.halt();
    assert_eq!(facts(&reloaded), facts(&log));
    assert_eq!(
        transcript(&log, u64::MAX, Arc::default()),
        [
            llm::Message::user("$ echo hi"),
            llm::Message::user("and again"),
            llm::Message::assistant("and again")
        ]
    );
}

/// #41: a crash after a gate's decisions but before what follows them:
/// recovery gates the call again without logging the decisions twice.
#[test]
fn a_gate_cut_off_by_a_crash_logs_its_decisions_once() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default())
            .with_in(ext::Tier::Guarded, &Strict)
            .unwrap(),
        openai(&Script::new(&[])),
    );
    let denied = decision(
        "policy",
        Some(2),
        Verdict::Permission {
            permission: Permission::Deny,
        },
    );
    let call = Body::ToolCall {
        tool: "bash".into(),
        input: json!({ "command": "echo hi" }).into(),
        id: None,
        card: None,
        by: Audience::Model,
    };
    let session = fixture.start(&[
        entry(1, user("$ echo hi")),
        entry(2, call),
        entry(3, denied.clone()),
    ]);
    let log = log_of(&session);
    session.halt();

    let decisions = log.iter().filter(|e| e.body == denied).count();
    assert_eq!(decisions, 1, "{log:#?}");
    assert!(
        matches!(
            &log[3].body,
            Body::ToolResult {
                call: 2,
                outcome: Outcome::Failed { .. },
                ..
            }
        ),
        "{log:#?}"
    );
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
    asked.submit("sleep 1").unwrap();
    settle(&asked);
    let started = Instant::now();
    let commands: Vec<_> = (0..2)
        .map(|_| {
            let session = fixture.start(&[]);
            session.submit("$ sleep 1").unwrap();
            session
        })
        .collect();
    asked.answer(2, true).unwrap();
    for session in &commands {
        finished(session, 2);
        session.halt();
    }
    finished(&asked, 2);
    let took = started.elapsed();
    asked.halt();
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

/// #18 `a_wait_survives_anything`: an ask waits in the inbox through a
/// stop, is delivered to each client that attaches, and runs once
/// approved.
#[test]
fn an_ask_waits_in_the_inbox_through_a_stop_and_runs_once_approved() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo approved").unwrap();
    // Waiting for an answer is not work.
    settle(&session);
    let asked = log_of(&session);
    assert_eq!(inbox(&asked), [2]);
    assert!(
        session.answer(1, true).is_err(),
        "only asked calls take answers"
    );
    session.halt();

    let resumed = fixture.start(&asked);
    let outbox = Outbox::new(Caps::NONE);
    resumed.follow(&outbox, asked.len() as u64);
    resumed.answer(2, true).unwrap();
    let log = finished(&resumed, 2);
    resumed.halt();

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
            DaemonMsg::Synced { seq: 3 },
            DaemonMsg::Ask { call: 2, tool, .. },
            ..
        ] if tool == "bash"),
        "{sent:#?}"
    );
    assert!(
        matches!(
            &bodies(&log)[..],
            [
                Body::Input { .. },
                Body::ToolCall { .. },
                Body::Decision {
                    d: Verdict::Permission {
                        permission: Permission::Ask,
                    },
                    ..
                },
                Body::Answer { call, value, .. },
                Body::ToolProcess { call: 2, .. },
                Body::ToolResult {
                    call: 2,
                    outcome: Outcome::Done { output },
                    card: Some(card),
                },
            ] if call.get() == 2 && value.raw() == "true"
                && output.value()["output"] == "approved\n" && card.note == "exit 0"
        ),
        "{log:#?}"
    );
    assert_eq!(inbox(&log), Vec::<u64>::new());
}

#[test]
fn a_refused_ask_answers_the_call_without_running_it() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo refused").unwrap();
    settle(&session);
    session.answer(2, false).unwrap();
    let log = finished(&session, 2);
    session.halt();
    assert_eq!(
        bodies(&log)[3..],
        [
            &Body::Answer {
                call: id(2),
                value: false.into(),
                origin: None,
            },
            &Body::ToolResult {
                call: 2,
                outcome: Outcome::Failed {
                    error: "the user did not approve this call".into()
                },
                card: None,
            },
        ]
    );
}

/// #10 `an_interrupt_never_starts_work`: an interrupt settles a call
/// waiting for approval; approving it after runs nothing.
#[test]
fn an_interrupt_while_asked_settles_the_call_without_running_it() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo ran").unwrap();
    settle(&session);
    session.interrupt_now();
    // Too late: it is settled already.
    assert!(session.answer(2, true).is_err());
    let log = finished(&session, 2);
    settle(&session);
    session.halt();
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
    assert_eq!(inbox(&log), Vec::<u64>::new());
}

/// An interrupt with nothing running stops nothing: not the next turn.
#[test]
fn an_interrupt_between_turns_is_not_kept_for_the_next() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.interrupt_now();
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(bodies(&log), [&user("hi"), &echo("hi")]);
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
        entry(1, user("first")),
        entry(2, echo("first")),
        entry(3, user("go on\nwith detail")),
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
        entry(
            7,
            Body::Wait {
                call: id(6),
                on: WaitOn::Approval,
            },
        ),
        entry(8, call("sleep")),
    ];
    let recap_of = |entries: &[Entry], since| {
        let ledger = protocol::ledger::Ledger::of(entries);
        let after = entries.partition_point(|e| e.seq <= ledger.turn_start());
        transcript::recap(
            &ledger,
            transcript::reply(&entries[after..]).as_deref(),
            since,
        )
    };
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

/// `/jobs` answers without a model turn; `shout` rewrites input and
/// refuses `no`; every `bash` call goes to the background.
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
            });
    }
}

#[test]
fn slash_commands_are_callables_and_input_hooks_rewrite_or_refuse() {
    let registry = Registry::default().with(&Extras).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("/jobs all of them").unwrap();
    session.submit("/nope").unwrap();
    session.submit("shout hi").unwrap();
    assert_eq!(session.submit("no"), Err("extras: not that".into()));
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let jobs = Body::ToolCall {
        tool: "jobs".into(),
        input: json!({ "args": "all of them" }).into(),
        id: None,
        card: None,
        by: Audience::User,
    };
    let ran = Body::ToolResult {
        call: 2,
        outcome: Outcome::Done {
            output: "all of them in 2".into(),
        },
        card: None,
    };
    // The command is an intent and an outcome under the input that ran
    // it; what came behind it waits, then is taken up in order.
    let shouted = decision(
        "extras",
        None,
        Verdict::Input {
            original: "shout hi".into(),
        },
    );
    assert_eq!(bodies(&log)[..2], [&user("/jobs all of them"), &jobs]);
    let answered = |log: &[Entry]| -> Vec<String> {
        (log.iter())
            .filter_map(|e| match &e.body {
                Body::Assistant { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(answered(&log), ["/nope", "SHOUT HI"], "{log:#?}");
    assert!(log.iter().any(|e| e.body == ran), "{log:#?}");
    assert!(log.iter().any(|e| e.body == shouted), "{log:#?}");
    // A command cut off by a crash is not run again; the input queued
    // behind it is taken up in order.
    let cut = fixture.start(&[
        entry(1, user("/jobs all of them")),
        entry(2, jobs),
        entry(3, queued("/nope")),
        entry(4, queued("SHOUT HI")),
    ]);
    let recovered = log_of(&cut);
    cut.halt();
    assert!(
        matches!(
            &bodies(&recovered)[4],
            Body::ToolResult {
                call: 2,
                outcome: Outcome::Interrupted { .. },
                ..
            }
        ),
        "{recovered:#?}"
    );
    assert_eq!(
        answered(&recovered),
        ["/nope", "SHOUT HI"],
        "{recovered:#?}"
    );
}

#[test]
fn the_model_gets_the_prompt_and_the_logged_context_first() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies[1..]);
    let fixture = Fixture::new(ext_chat::CHAT, echoing(&Policy::default()), openai(&script));
    let session = fixture.start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

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
    assert_eq!(tools, ["bash", "edit", "task", "sleep"]);
    assert_eq!(
        transcript(&log, u64::MAX, fixture.0.registry.get())[0],
        llm::Message::user(text.clone())
    );
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
        .submit(&format!("$ mkdir -p {dir} && cd {dir}"))
        .unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let again = fixture.start(&log);
    again.submit("$ pwd").unwrap();
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

    again.submit("$ sleep 100").unwrap();
    until(&again, "started", |log| {
        log.iter()
            .filter(|e| matches!(e.body, Body::ToolProcess { .. }))
            .count()
            == 3
    });
    again.interrupt_now();
    settle(&again);
    let log = log_of(&again);
    again.halt();
    let _ = std::fs::remove_dir(std::env::temp_dir().join(&dir));
    let result = log.iter().rev().find_map(|e| match &e.body {
        Body::ToolResult {
            outcome: Outcome::Done { output },
            ..
        } => Some(output),
        _ => None,
    });
    assert!(
        result.is_some_and(|output| output.value()["aborted"] == true),
        "{log:#?}"
    );
    assert!(matches!(
        log.last().map(|e| &e.body),
        Some(Body::Interrupt { .. })
    ));
}

#[test]
fn background_processes_keep_the_session_and_end_with_it() {
    let registry = echoing(&Policy::default()).with(&Extras).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("$ sleep 100").unwrap();
    settle(&session);
    let pgid = until(&session, "logged the job", |log| {
        log.iter().any(|e| {
            matches!(
                e.body,
                Body::ToolProcess {
                    background: true,
                    ..
                }
            )
        })
    })
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
    session.halt();
    let deadline = Instant::now() + Duration::from_secs(5);
    while sys::Group(pgid).alive() || session.has_jobs() {
        assert!(Instant::now() < deadline, "the job outlived its session");
        thread::sleep(Duration::from_millis(10));
    }
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
            rt::Timer::after(Duration::from_millis(5)).await;
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
    ) -> llm::http::BoxFuture<'static, io::Result<llm::http::Response>> {
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
    session.submit("Call the tool, then answer.").unwrap();
    until(&session, "called the tool", tool_running);
    session.steer("Say it twice.").unwrap();
    session.submit("And then this.").unwrap();
    release.store(true, Ordering::SeqCst);
    let log = until(&session, "took the queued input", |log| {
        log.iter()
            .filter(|e| matches!(e.body, Body::Take { .. }))
            .count()
            == 2
    });
    session.halt();

    let after_tool: Vec<_> = bodies(&log)
        .into_iter()
        .skip_while(|b| !matches!(b, Body::ToolResult { .. }))
        .collect();
    assert!(
        matches!(
            &after_tool[..],
            [
                Body::ToolResult { .. },
                Body::Take { input: steered },
                Body::ModelCall { .. },
                Body::ModelResult { .. },
                Body::Take { input: then },
                ..
            ] if steered.get() == 5 && then.get() == 6
        ),
        "{log:#?}"
    );
    assert!(matches!(
        &log[4].body,
        Body::Input {
            mode: Mode::Steer,
            taken: false,
            ..
        }
    ));
    let input = script.sent()[1]["input"].to_string();
    assert!(input.contains("Say it twice."), "{input}");
}

#[test]
fn an_interrupt_stops_the_model_call_and_ends_the_turn() {
    let models = Models {
        settings: crate::config::Settings::new(gpt_home(), None),
        http: Arc::new(Silent),
    };
    let fixture = Fixture::new(ext_chat::CHAT, Registry::default(), models);
    let session = fixture.start(&[]);
    session.submit("hi").unwrap();
    until(&session, "called the model", |log| {
        log.iter().any(|e| matches!(e.body, Body::ModelCall { .. }))
    });
    session.interrupt_now();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(
        bodies(&log)[2..],
        [
            &Body::ModelResult {
                call: 2,
                outcome: Outcome::Interrupted { output: None }
            },
            &Body::INTERRUPT,
        ]
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
    session.submit("Call the tool, then answer.").unwrap();
    until(&session, "called the tool", tool_running);
    session.interrupt_now();
    settle(&session);
    let log = log_of(&session);
    session.halt();

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
    let again = fixture.start(&log);
    let reloaded = log_of(&again);
    again.halt();
    assert_eq!(facts(&reloaded), facts(&log));
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
    session.halt();
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
        Registry::default().with(&ext_settings::Settings).unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    for text in [
        "/model",
        "/model anthropic:claude-x",
        "/model",
        "/model reset",
    ] {
        session.submit(text).unwrap();
        settle(&session);
    }
    let log = log_of(&session);
    let results: Vec<Value> = log
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolResult {
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
            set("this session, at entry 6", "anthropic:claude-x"),
            set("this session, at entry 6", "anthropic:claude-x"),
            set("the home config.toml", "gpt-5.6"),
        ]
    );
    let chosen = Body::Ext {
        ext: "settings".into(),
        state: "model".into(),
        data: "anthropic:claude-x".into(),
    };
    assert_eq!(log[5].body, chosen, "{log:#?}");
    assert_eq!(session.model_spec(), "gpt-5.6");
    session.halt();

    // The session's own setting lasts in its log.
    let again = fixture.start(&log[..7]);
    assert_eq!(again.model_spec(), "anthropic:claude-x");
    again.halt();
}

/// Every command is an extension's: without one, `/model` is input like
/// any other, and a session advertises no commands.
#[test]
fn the_kernel_knows_no_command_names() {
    let registry = Registry::default();
    assert_eq!(commands(&registry), []);
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("/model gpt").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(bodies(&log), [&user("/model gpt"), &echo("/model gpt")]);

    let with = Registry::default().with(&ext_settings::Settings).unwrap();
    let names: Vec<_> = commands(&with).into_iter().map(|c| c.name).collect();
    assert_eq!(names, ["model"]);
}

/// #24 `a_step_runs_on_one_registry_generation`.
#[test]
fn a_new_registry_generation_applies_from_the_next_step() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let reloaded = echoing(&Policy::default()).with(&Extras).unwrap();
    fixture.0.registry.replace(reloaded);
    session.submit("/jobs now").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    let after: Vec<_> = bodies(&log).into_iter().skip(2).collect();
    assert!(
        matches!(
            &after[..],
            [
                Body::Extensions { installed },
                Body::Input { .. },
                Body::ToolCall { by: Audience::User, .. },
                Body::ToolResult { outcome: Outcome::Done { .. }, .. },
            ] if installed.last().is_some_and(|i| i.name == "extras")
        ),
        "{log:#?}"
    );
}

#[test]
fn input_waits_its_turn_behind_a_call_waiting_for_approval() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo approved").unwrap();
    settle(&session);
    session.submit("next").unwrap();
    let waiting = log_of(&session);
    session.answer(2, true).unwrap();
    let log = until(&session, "took the input", |log| {
        log.iter().any(|e| e.body == echo("next"))
    });
    session.halt();

    assert_eq!(waiting.last().map(|e| &e.body), Some(&queued("next")));
    let after: Vec<_> = bodies(&log)
        .into_iter()
        .skip_while(|b| !matches!(b, Body::ToolResult { .. }))
        .skip(1)
        .collect();
    let next = Seq::new(4).unwrap();
    assert_eq!(after, [&Body::Take { input: next }, &echo("next")]);
}

/// Input that arrives while a loaded session recovers is in the log as
/// waiting when recovery looks for what waits: it is taken up once, after
/// what was waiting before the load.
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
        session.submit("two").unwrap();
        settle(&session);
        let log = log_of(&session);
        session.halt();
        let said: Vec<_> = bodies(&log)
            .into_iter()
            .filter_map(|b| match b {
                Body::Assistant { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(said, ["one", "left", "two"], "{log:#?}");
    }
}

/// #13: two clients answer one ask at once: the first answer wins, the
/// other is told the call waits no more, and one answer is logged.
#[test]
fn the_first_answer_wins() {
    let fixture = asking();
    let session = fixture.start(&[]);
    session.submit("$ echo once").unwrap();
    settle(&session);
    let answers: Vec<_> = [true, false]
        .map(|approved| {
            let session = Arc::clone(&session);
            thread::spawn(move || session.answer(2, approved))
        })
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    let log = finished(&session, 2);
    session.halt();
    assert_eq!(
        answers.iter().filter(|a| a.is_ok()).count(),
        1,
        "{answers:?}"
    );
    let logged = log
        .iter()
        .filter(|e| matches!(e.body, Body::Answer { .. }))
        .count();
    assert_eq!(logged, 1, "{log:#?}");
}

/// Denies `bash` by panicking.
struct Broken;

impl Extension for Broken {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<point::Permission>(|_, call| {
            assert!(
                call.tool != ext_shell::Bash::NAME,
                "no policy for {}",
                call.tool
            );
            Permission::Allow
        });
    }
}

/// #43: a gate whose handler panics denies the call, logged with why;
/// the session goes on.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "dev builds' cranelift backend cannot unwind through these frames: \
              CARGO_PROFILE_DEV_CODEGEN_BACKEND=llvm cargo nextest run -p engine --run-ignored all"
)]
fn a_failing_gate_fails_closed() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default())
            .with_in(ext::Tier::Guarded, &Broken)
            .unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    session.submit("$ echo ran").unwrap();
    let log = finished(&session, 2);
    session.submit("still here").unwrap();
    settle(&session);
    let after = log_of(&session);
    session.halt();
    assert!(
        matches!(&log[2].body, Body::Decision { d: Verdict::Block { reason }, .. } if reason.contains("panicked")),
        "{log:#?}"
    );
    assert!(
        matches!(&log[3].body, Body::ToolResult { outcome: Outcome::Failed { error }, .. } if error.contains("no policy for bash")),
        "{log:#?}"
    );
    assert!(
        !log.iter()
            .any(|e| matches!(e.body, Body::ToolProcess { .. }))
    );
    assert_eq!(after.last().map(|e| &e.body), Some(&echo("still here")));
}

/// Panics on `boom`, the first time it is given it.
struct Panicky;

static BOOMED: AtomicBool = AtomicBool::new(false);

impl ext::Agent for Panicky {
    fn step(&mut self, input: ext::Input) -> ext::Actions {
        match input {
            ext::Input::User { text } => {
                let first = text == "boom" && !BOOMED.swap(true, Ordering::SeqCst);
                assert!(!first, "boom");
                ext::Actions::from_elem(ext::Action::Say(text), 1)
            }
            _ => ext::Actions::new(),
        }
    }
}

struct Panics;

impl Extension for Panics {
    const NAME: &'static str = "panics";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.agent("panicky", || Box::new(Panicky));
    }
}

/// #63: a step that panics is a bug the session survives: what it did is
/// dropped, the asker told, and the session resumes from its log.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "dev builds' cranelift backend cannot unwind through these frames: \
              CARGO_PROFILE_DEV_CODEGEN_BACKEND=llvm cargo nextest run -p engine --run-ignored all"
)]
fn a_kernel_panic_is_a_resume() {
    let registry = Registry::default().with(&Panics).unwrap();
    let fixture = Fixture::new("panicky", registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let boom = session.submit("boom");
    session.submit("again").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert!(boom.is_err(), "{boom:?}");
    assert_eq!(
        facts(&log),
        [
            (1, &user("hi")),
            (2, &echo("hi")),
            (3, &user("again")),
            (4, &echo("again"))
        ]
    );
}

/// #57: the entries of one step are committed together: one write and
/// one fsync, however many.
#[test]
fn one_fsync_per_step() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let older = Body::Extensions {
        installed: vec![Installed {
            name: "shell".into(),
            version: 0,
        }],
    };
    let bash = Body::ToolCall {
        tool: "bash".into(),
        input: json!({ "command": "make" }).into(),
        id: None,
        card: None,
        by: Audience::Model,
    };
    let entries = [entry(1, older), entry(2, user("$ make")), entry(3, bash)];
    let header = fixture.header();
    let mut created = probe.store.create(&header).unwrap();
    for e in &entries {
        let e = Entry::new(e.seq, created.writer.epoch(), e.body.clone());
        created.writer.log.append_all(&[e]).unwrap();
    }
    drop(created);
    let session = Live::start(&header, &fixture.0, Open::Load, Owner::None).unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    // The recovered step: the extensions noted, the call cut off answered.
    assert!(
        matches!(
            &bodies(&log)[3..],
            [Body::Extensions { .. }, Body::ToolResult { call: 3, .. }]
        ),
        "{log:#?}"
    );
    assert_eq!(*lock(&probe.batches), [2]);
}
