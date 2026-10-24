//! A live session against its effects: a scripted model, real tools and
//! stores, crashes cut at every fact.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Weak};
use std::thread;
use std::time::Duration;

use std::task::Poll;

use crate::store::{Encoded, Loaded, Log, LogStore, Memory, Snapshot};
use ext::{Answer, Extension, Recover, Registrar, Replay, Rewrite, Tool, point};
use ext_policy::Policy;
use llm::http::Http;
use protocol::{Audience, CallId, Card, Epoch, Installed, Permission, Seq, Verdict};
use serde_json::{Value, json};

use super::*;

mod admission;
mod bench;
mod durable;
mod interrupt;
mod invocation;
mod later;
mod log;
mod parsed;
mod retry;
mod scenarios;
mod side;

fn entry(seq: u64, body: Body) -> Entry {
    Entry::new(Seq::new(seq).unwrap(), Epoch(0), body)
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
        segments: text.into(),
        mode: Mode::Queue,
        taken: true,
        origin: None,
    }
}

/// Input that waits to be taken up.
fn queued(text: &str) -> Body {
    Body::Input {
        segments: text.into(),
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
    (Ledger::of(entries).inbox()).map(CallId::get).collect()
}

/// The conversation before `before`, as `registry`'s tools present it.
fn transcript(entries: &[Entry], before: u64, registry: Arc<Registry>) -> Vec<llm::Message> {
    let before = entries.iter().take_while(|e| e.seq.get() < before);
    Transcript::of(before, registry, "")
        .into_messages()
        .to_vec()
}

/// What entries say, whichever lease wrote them.
fn facts(entries: &[Entry]) -> Vec<(u64, &Body)> {
    entries.iter().map(|e| (e.seq.get(), &e.body)).collect()
}

/// The tool rig's recordings call.
struct HarborLabel;

/// The tool's input: nothing, under the schema the recordings offer it
/// with.
#[derive(serde::Deserialize)]
struct Nothing {}

impl ext::Schema for Nothing {
    fn schema(out: &mut String) {
        out.push_str(r#"{"type":"object","properties":{},"required":[]}"#);
    }
}

impl Tool for HarborLabel {
    type In = Nothing;
    type Out = String;

    const NAME: &'static str = "lookup_harbor_label";
    const DESCRIPTION: &'static str = "Return the alpha signal marker.";
    const REPLAY: Replay = Replay::Safe;

    fn call(&self, _: &Nothing, _: &Cx) -> Result<String, String> {
        Ok("crimson-harbor".to_owned())
    }

    fn to_model(label: &Json) -> llm::Output {
        llm::Output::Text(label.parse().unwrap())
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
                .body(llm::http::body([body.into()]))
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

struct Fixture(Arc<Shared>, Arc<Counting>, Arc<Daemon>);

struct TestingLive(Arc<Live>, Arc<Daemon>);
impl std::ops::Deref for TestingLive {
    type Target = Arc<Live>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl TestingLive {
    fn halt(&self) {
        self.0.halt();
        self.1.wait("owned admissions drained", PATIENCE, || {
            self.1.slots().iter().all(|slot| {
                slot.header.parent.is_none_or(|p| p.session != self.0.id)
                    || !matches!(*lock(&slot.residency), crate::Residency::Loading { .. })
            })
        });
        for live in self.1.loaded() {
            if live.parent.is_some_and(|p| p.session == self.0.id) {
                live.halt();
            }
        }
    }
}
impl Drop for TestingLive {
    fn drop(&mut self) {
        self.halt();
    }
}

/// The runtime sessions under test run on, counting its blocking hops.
struct Counting(rt::Threads, AtomicUsize);

impl rt::Runtime for Counting {
    fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
        self.0.spawn(task);
    }

    fn spawn_blocking(
        &self,
        class: rt::BlockingClass,
        bytes: usize,
        work: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, io::Result<()>> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.spawn_blocking(class, bytes, work)
    }

    fn spawn_flush(
        &self,
        work: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, io::Result<()>> {
        self.0.spawn_flush(work)
    }

    fn reactor(&self) -> rt::Reactor {
        self.0.reactor()
    }
}

/// The agents sessions under test run.
struct Agents;

impl Extension for Agents {
    const NAME: &'static str = "agents";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.agent(ext_fixtures::ECHO, || Box::new(ext_fixtures::Echo));
    }
}

impl Fixture {
    fn configure(self, configure: impl FnOnce(&mut Shared)) -> Self {
        let Self(shared, runtime, daemon) = self;
        drop(shared);
        let mut daemon = Arc::try_unwrap(daemon).ok().expect("unstarted fixture");
        configure(Arc::get_mut(&mut daemon.shared).unwrap());
        let daemon = Arc::new_cyclic(|weak| {
            Arc::get_mut(&mut daemon.shared).unwrap().daemon = weak.clone();
            daemon
        });
        Self(Arc::clone(&daemon.shared), runtime, daemon)
    }

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
        // One worker: no effect may hold it.
        let counting = Arc::new(Counting(rt::Threads::new(1), AtomicUsize::new(0)));
        let rt: rt::Rt = Arc::clone(&counting) as _;
        let daemon = Arc::new_cyclic(|daemon| Daemon {
            shared: Arc::new(Shared {
                http_clients: std::sync::OnceLock::new(),
                rt,
                store,
                catalog: crate::catalog::Catalog::memory(),
                registry: ext::host::Current::new(registry),
                models,
                barriers: kernel::Barriers::default(),
                build: "test".into(),
                spill: std::env::temp_dir().join(format!("a-spill-{}", std::process::id())),
                loaded: Mutex::default(),
                daemon: daemon.clone(),
            }),
            slots: Mutex::default(),
            admission: Mutex::new(true),
            clients: AtomicUsize::new(0),
            pulse: rt::Notify::new(),
            jobs: rt::Notify::new(),
            watchers: Mutex::default(),
            stopping: Mutex::default(),
            started: 0,
        });
        Self(Arc::clone(&daemon.shared), counting, daemon)
    }

    /// The blocking hops taken so far.
    fn hops(&self) -> usize {
        self.1.1.load(Ordering::SeqCst)
    }

    fn header() -> Header {
        Header {
            id: SessionId::new(),
            workspace: std::env::temp_dir(),
            parent: None,
        }
    }

    /// A new session.
    fn create(&self) -> TestingLive {
        let header = Self::header();
        let slot = self.2.new_slot(header.clone()).unwrap();
        let live = Live::start(&header, &self.0, Open::Create, Owner::None).unwrap();
        let live = self.2.started(&slot, Ok(live)).unwrap();
        TestingLive(live, Arc::clone(&self.2))
    }

    /// A session whose log holds `entries`, started as after a crash, once
    /// it has recovered.
    fn start(&self, entries: &[Entry]) -> TestingLive {
        let session = self.load(entries);
        settle(&session);
        session
    }

    /// [`Fixture::start`], without waiting for recovery.
    fn load(&self, entries: &[Entry]) -> TestingLive {
        let header = Self::header();
        let mut created = self.0.store.create(&header).unwrap();
        for e in entries {
            let e = Entry::new(e.seq, Epoch(created.writer.epoch()), e.body.clone());
            created.writer.append_raw(std::slice::from_ref(&e)).unwrap();
        }
        drop(created);
        let slot = self.2.new_slot(header.clone()).unwrap();
        let live = Live::start(&header, &self.0, Open::Load, Owner::None).unwrap();
        let live = self.2.started(&slot, Ok(live)).unwrap();
        TestingLive(live, Arc::clone(&self.2))
    }
}

impl Live {
    fn steer(&self, text: &str) -> Result<(), String> {
        let steer = Request::Input {
            segments: text.into(),
            mode: Mode::Steer,
        };
        rt::block_on(self.request(steer, None))
    }

    fn answer(&self, call: u64, approved: bool) -> Result<(), String> {
        let answer = Request::Answer {
            call: CallId::new(call).unwrap(),
            value: Json::encode(&approved).unwrap(),
        };
        rt::block_on(self.request(answer, None))
    }

    fn interrupt_now(&self) {
        let _ = rt::block_on(self.request(Request::Interrupt, None));
    }

    /// Stops the actor and waits for it to end.
    fn halt(&self) {
        self.stop();
        rt::block_on(self.ended.wait());
    }

    fn follow(&self, outbox: &Outbox, since: u64) {
        rt::block_on(self.attach(outbox.clone(), since));
    }

    fn model_spec(&self) -> String {
        self.session.model_spec(&self.session.shared.registry.get())
    }
}

/// How long a test waits for the session before giving up.
const PATIENCE: Duration = Duration::from_secs(10);

/// Waits until the session has taken what it was sent and has no work.
fn settle(session: &Live) {
    session.barrier();
    session.wait("settled", PATIENCE, || !session.has_work());
    session.barrier();
}

fn log_of(session: &Live) -> Vec<Entry> {
    session.frames.entries().unwrap().into_iter().collect()
}

/// Waits until `done` holds for the session's log.
fn until(session: &Live, what: &str, done: impl Fn(&[Entry]) -> bool + Sync) -> Vec<Entry> {
    let mut log = Vec::new();
    session.wait(what, PATIENCE, || {
        log = log_of(session);
        done(&log)
    });
    log
}

/// Waits for call `call`'s outcome to be committed.
fn finished(session: &Live, call: u64) -> Vec<Entry> {
    until(session, &format!("finished call {call}"), |log| {
        log.iter()
            .any(|e| matches!(e.body, Body::ToolResult { call: c, .. } if c.get() == call))
    })
}

/// A store whose logs keep the size of each batch they are handed, fail
/// while told to, and wait while `held` is locked.
#[derive(Clone, Default)]
struct Probe {
    store: Arc<Memory>,
    batches: Arc<Mutex<Vec<usize>>>,
    failing: Arc<AtomicBool>,
    held: Arc<Mutex<()>>,
    /// Appends waiting for `held`.
    waiting: Arc<AtomicUsize>,
    /// Each batch, and whether it was made durable.
    written: Arc<Mutex<Vec<Written>>>,
    /// The entries loads read, and the snapshots kept, by what they cover.
    read: Arc<AtomicUsize>,
    snapshots: Arc<Mutex<Vec<u64>>>,
    observe_write: Arc<Mutex<Option<Box<WriteObserver>>>>,
}

/// A batch handed to a log, and whether it was made durable.
type Written = (Vec<Entry>, bool);
type WriteObserver = dyn Fn(&[Entry], bool) + Send + Sync;

struct Probed(Box<dyn Log>, Probe);

impl Probed {
    fn write(&mut self, entries: &[Entry], lines: &Encoded, durable: bool) -> io::Result<()> {
        self.1.waiting.fetch_add(1, Ordering::SeqCst);
        drop(lock(&self.1.held));
        self.1.waiting.fetch_sub(1, Ordering::SeqCst);
        if self.1.failing.load(Ordering::SeqCst) {
            return Err(io::Error::other("disk full"));
        }
        lock(&self.1.batches).push(entries.len());

        lock(&self.1.written).push((entries.to_vec(), durable));
        let observe = lock(&self.1.observe_write);
        if let Some(observe) = &*observe {
            observe(entries, false);
        }
        self.0.append_all(entries, lines)?;
        if let Some(observe) = &*observe {
            observe(entries, true);
        }
        Ok(())
    }
}

impl Log for Probed {
    fn append_all(&mut self, entries: &[Entry], lines: &Encoded) -> io::Result<()> {
        self.write(entries, lines, true)
    }

    fn write_all(&mut self, entries: &[Entry], lines: &Encoded) -> io::Result<()> {
        self.write(entries, lines, false)
    }

    fn sync(&mut self) -> io::Result<()> {
        lock(&self.1.written).push((Vec::new(), true));
        self.0.sync()
    }

    fn snapshot(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        lock(&self.1.snapshots).push(snapshot.covers);
        self.0.snapshot(snapshot)
    }
}

impl LogStore for Probe {
    fn prefix(&self, id: SessionId, seq: u64) -> io::Result<Vec<Entry>> {
        self.store.prefix(id, seq)
    }
    fn suffix(
        &self,
        id: SessionId,
        cursor: &mut ext::LogCursor,
        through: u64,
    ) -> io::Result<(bool, Vec<Entry>)> {
        self.store.suffix(id, cursor, through)
    }
    fn import(&self, header: &Header, entries: &[Entry]) -> io::Result<()> {
        self.store.import(header, entries)
    }

    fn create(&self, header: &Header) -> io::Result<Loaded> {
        let mut loaded = self.store.create(header)?;
        loaded.writer.log = Box::new(Probed(loaded.writer.log, self.clone()));
        Ok(loaded)
    }

    fn headers(&self) -> io::Result<Vec<Header>> {
        self.store.headers()
    }

    fn load(
        &self,
        id: SessionId,
        restore: &mut dyn FnMut(Snapshot) -> bool,
        each: &mut dyn FnMut(Entry),
    ) -> io::Result<Loaded> {
        let read = |e| {
            self.read.fetch_add(1, Ordering::SeqCst);
            each(e);
        };
        let mut loaded = self.store.load(id, restore, &mut { read })?;
        loaded.writer.log = Box::new(Probed(loaded.writer.log, self.clone()));
        Ok(loaded)
    }

    fn entries(&self, id: SessionId) -> io::Result<Vec<Entry>> {
        self.store.entries(id)
    }

    fn count(&self, id: SessionId) -> io::Result<u64> {
        self.store.count(id)
    }
}

/// #9 `a_failed_commit_is_resume`: input commits on arrival, before the
/// turn it starts; input that cannot be logged is refused and gone, seq
/// and all, and the session goes on from its log.
#[test]
fn pressure_retains_cached_projections_while_a_process_is_owned() {
    let fixture = Fixture::new(ext::LOOP, Registry::default(), openai(&Script::new(&[])));
    let session = fixture.start(&[entry(1, echo("cached message"))]);
    let process: Arc<dyn ext::RunningProcess> = Arc::new(admission::StaleProcess);
    lock(&session.session.jobs).push(Arc::clone(&process));
    assert!(session.has_jobs());
    assert!(!session.has_work());
    assert_eq!(session.summary().activity, Activity::Idle);
    assert_eq!(session.projection_cache(true), [true; 3]);
    session.reclaim();
    assert_eq!(session.projection_cache(false), [true; 3]);
    assert!(Arc::ptr_eq(&lock(&session.session.jobs)[0], &process));
    lock(&session.session.jobs).clear();
    session.reclaim();
    assert_eq!(session.projection_cache(false), [false; 3]);
    session.halt();
}

#[test]
fn pressure_discards_a_cached_snapshot_without_changing_its_rebuild() {
    let script = Script::new(&[]);
    let fixture = Fixture::new(ext::LOOP, Registry::default(), openai(&script));
    let entries: Vec<_> = (1..=SNAPSHOT_GAP + 1)
        .map(|seq| entry(seq, echo("cached message")))
        .collect();
    let session = fixture.start(&entries);
    let attach = || {
        let outbox = Outbox::new();
        session.follow(&outbox, 0);
        std::iter::from_fn(|| outbox.try_next())
            .map(|frame| frame.as_bytes().to_vec())
            .collect::<Vec<_>>()
    };
    let first = attach();
    let built = fixture.hops();
    assert_eq!(attach(), first);
    assert_eq!(fixture.hops(), built, "an unchanged snapshot is reused");
    session.reclaim();
    session.barrier();
    assert_eq!(attach(), first);
    assert!(
        fixture.hops() > built,
        "pressure released the cached projection"
    );
    session.halt();
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
    // The extensions it was created with, then a batch a step.
    assert_eq!(*lock(&probe.batches), [1, 2, 2]);
    assert!(matches!(log[0].body, Body::Extensions { .. }));
    assert_eq!(
        facts(&log[1..]),
        [
            (2, &user("hi")),
            (3, &echo("hi")),
            (4, &user("again")),
            (5, &echo("again"))
        ]
    );
}

#[test]
fn the_model_calls_a_tool_and_answers_from_its_result() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies);
    let tools = Registry::default().with(&Harbor).unwrap();
    let fixture = Fixture::new(ext::LOOP, tools, openai(&script));
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
                Body::ModelResult { r#for: None, failure: None, call: a, outcome: Outcome::Done { .. } },
                Body::ToolCall { tool, id: Some(id), .. },
                Body::ToolResult { call: b, outcome: Outcome::Done { output }, card: None },
                Body::ModelCall { .. },
                Body::ModelResult { r#for: None, failure: None, call: c, outcome: Outcome::Done { output: answer } },
            ] if (a.get(), b.get(), c.get()) == (2, 4, 6) && tool == "lookup_harbor_label" && id == call_id && output.parse::<Value>().unwrap() == "crimson-harbor"
                && answer.parse::<Value>().unwrap()["message"]["content"][1]["text"] == "The harbor label is crimson-harbor."
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
        let fixture = Fixture::new(ext::LOOP, registry.with(&retry).unwrap(), openai(&script));
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
            ext::LOOP,
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
    let fixture = Fixture::new(ext::LOOP, Registry::default(), models);
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
        ] if model == crate::config::DEFAULT_MODEL => error.clone(),
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
        http: Arc::new(llm::Ureq::new(rt, llm::Redirects::Follow)),
    };
    let fixture = Fixture::new(
        ext::LOOP,
        Registry::default().with(&Harbor).unwrap(),
        models,
    );
    let session = fixture.start(&[]);
    session
        .submit("Call lookup_harbor_label, then tell me the label it returned.")
        .unwrap();
    session.wait("answered", Duration::from_secs(120), || !session.has_work());
    let log = log_of(&session);
    session.halt();
    assert!(
        log.iter().any(|e| matches!(
            &e.body,
            Body::ToolResult { outcome: Outcome::Done { output }, .. } if output.parse::<Value>().unwrap() == "crimson-harbor"
        )),
        "{log:#?}"
    );
}

#[test]
fn a_fired_watch_wakes_the_model_with_the_childs_result() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let fixture =
        |script: &Arc<Script>| Fixture::new(ext::LOOP, Registry::default(), openai(script));
    let child = SessionId::new();
    let script = Script::new(&replies[1..]);
    let session = fixture(&script).load(&[entry(1, Body::Watch { child })]);
    let found = Outcome::Done {
        output: Json::encode("found it").unwrap(),
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
                Body::Fired { watch, outcome },
                Body::ModelCall { .. },
                Body::ModelResult { r#for: None, failure: None, call, outcome: Outcome::Done { .. } },
            ] if (watch.get(), call.get()) == (1, 3) && *outcome == found
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
            image: None,
        }],
    };
    let upgraded = fixture.start(&[entry(1, older)]);
    let noted = log_of(&upgraded);
    upgraded.halt();

    assert_eq!(unchanged, []);
    assert_eq!(
        bodies(&noted)[1..],
        [&Body::Extensions {
            installed: fixture.0.registry.get().installed()
        }]
    );
}

/// Every extension and the fixtures, with `policy` as given.
fn echoing(policy: &Policy) -> Registry {
    crate::tests::extensions(policy).unwrap()
}

/// In place of the `policy` extension: denies shell commands, and has the
/// agent go on once after it first stops.
#[derive(Default)]
struct Strict(Arc<AtomicBool>);

impl Extension for Strict {
    const NAME: &'static str = "policy";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        let continued = Arc::clone(&self.0);
        r.on::<point::Permission>(|_, call| {
            if call.tool == ext_shell::Bash::NAME {
                Permission::Deny
            } else {
                Permission::Allow
            }
        })
        .on::<point::OnStop>(move |_, ()| {
            (!continued.swap(true, Ordering::SeqCst)).then(|| "and again".to_owned())
        });
    }
}

#[test]
fn hook_decisions_are_logged_after_their_call_and_replay_to_the_same_state() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default())
            .with_in(ext::Tier::Guarded, &Strict::default())
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
                invoke: None,
                tool: "bash".into(),
                input: Json::encode(&json!({ "command": "echo hi" })).unwrap(),
                id: None,
                card: Some(Card {
                    title: "$ echo hi".into(),
                    ..Card::default()
                }),
                by: Audience::Model,
            },
            &decision("policy", Some(2), denied),
            &Body::ToolResult {
                call: CallId::new(2).unwrap(),
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

/// The calls of one reply end in any order, and are logged as they end;
/// the model reads their results in the order it made the calls, so the
/// request is the same however the run went.
#[test]
fn results_are_logged_in_call_order() {
    let call = |id: &str| Body::ToolCall {
        invoke: None,
        tool: "sleep".into(),
        input: Json::encode(&json!({ "seconds": 1 })).unwrap(),
        id: Some(id.into()),
        card: None,
        by: Audience::Model,
    };
    let result = |call: u64| Body::ToolResult {
        call: CallId::new(call).unwrap(),
        outcome: Outcome::Done {
            output: Json::encode(&json!({ "slept": call })).unwrap(),
        },
        card: None,
    };
    let log = [
        entry(1, call("a")),
        entry(2, call("b")),
        entry(3, call("c")),
        entry(4, result(3)),
        entry(5, result(1)),
        entry(6, result(2)),
    ];
    let messages = transcript(&log, u64::MAX, Arc::default());
    let [llm::Message::Tool(answers)] = &messages[..] else {
        panic!("{messages:#?}");
    };
    let ids: Vec<&str> = answers.results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
}

/// #41: a crash after a gate's decisions but before what follows them:
/// recovery gates the call again without logging the decisions twice.
#[test]
fn a_gate_cut_off_by_a_crash_logs_its_decisions_once() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default())
            .with_in(ext::Tier::Guarded, &Strict::default())
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
        invoke: None,
        tool: "bash".into(),
        input: Json::encode(&json!({ "command": "echo hi" })).unwrap(),
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
                call,
                outcome: Outcome::Failed { .. },
                ..
            } if call.get() == 2
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

/// An interrupt stepped in the burst that approves a call stops that
/// call: its cancel is carried out after the call starts, not before, when
/// there would be nothing of it to stop.
#[test]
fn an_interrupt_in_the_burst_that_approves_a_call_stops_it() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        echoing(&Policy::ask(&["bash"])),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session.submit("$ sleep 30").unwrap();
    settle(&session);
    let [call] = inbox(&log_of(&session))[..] else {
        panic!("not asked");
    };
    // The actor waits in a commit while the answer and the interrupt
    // queue behind it.
    let held = lock(&probe.held);
    let queued = session.ask(
        Request::Input {
            segments: "then".into(),
            mode: Mode::Queue,
        },
        None,
    );
    while probe.waiting.load(Ordering::SeqCst) == 0 {
        thread::yield_now();
    }
    let approve = Request::Answer {
        call: CallId::new(call).unwrap(),
        value: Json::encode(&true).unwrap(),
    };
    let asked = [
        queued,
        session.ask(approve, None),
        session.ask(Request::Interrupt, None),
    ];
    drop(held);
    for answer in asked {
        rt::block_on(answer).unwrap();
    }
    let log = finished(&session, call);
    session.halt();
    let outcome = log.iter().find_map(|e| match &e.body {
        Body::ToolResult {
            call: c, outcome, ..
        } if c.get() == call => Some(outcome),
        _ => None,
    });
    // Aborted before it ran, or killed as it started: never run through.
    assert!(
        !matches!(outcome, Some(Outcome::Done { output }) if output.parse::<Value>().unwrap()["exit"] == 0),
        "{log:#?}"
    );
}

/// #18 `a_wait_survives_anything`: an ask waits in the inbox through a
/// stop, every client that attaches folds it from the log, and it runs
/// once approved.
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
    let outbox = Outbox::new();
    resumed.follow(&outbox, 0);
    let mut view = SessionView::default();
    for frame in std::iter::from_fn(|| outbox.try_next()) {
        let msg: DaemonMsg = protocol::read_frame(&mut frame.as_bytes())
            .unwrap()
            .unwrap();
        view.fold(&msg);
    }
    assert_eq!(view.state.inbox().next(), CallId::new(2));
    assert!(view.recap.is_some_and(|r| r.asks == 1));
    resumed.answer(2, true).unwrap();
    let log = finished(&resumed, 2);
    resumed.halt();

    // A slow shell may stream progress before finishing the same call.
    let settled: Vec<_> = bodies(&log)
        .into_iter()
        .filter(|body| !matches!(body, Body::Progress { .. }))
        .collect();
    assert!(
        matches!(
            &settled[..],
            [
                Body::Input { .. },
                Body::ToolCall { .. },
                Body::Decision {
                    d: Verdict::Permission {
                        permission: Permission::Ask(_),
                    },
                    ..
                },
                Body::Answer { call, value, .. },
                Body::ToolProcess { call: ran, .. },
                Body::ToolResult {
                    call: done,
                    outcome: Outcome::Done { output },
                    card: Some(card),
                },
            ] if [call, ran, done].iter().all(|c| c.get() == 2) && value.raw() == "true"
                && output.parse::<Value>().unwrap()["output"] == "approved\n" && card.note == "exit 0"
        ),
        "{log:#?}"
    );
    assert_eq!(inbox(&log), Vec::<u64>::new());
}

/// `/jobs` answers without a model turn; `shout` rewrites input and
/// refuses `no`; every `bash` call goes to the background.
struct Extras;

struct Jobs;

impl Tool for Jobs {
    type In = ext::Args;
    type Out = Value;

    const NAME: &'static str = "jobs";
    const DESCRIPTION: &'static str = "Lists jobs.";
    const AUDIENCE: Audience = Audience::User;

    fn call(&self, i: &ext::Args, cx: &Cx) -> Result<Value, String> {
        Ok(format!("{} in {}", i.args, cx.call).into())
    }
}

impl Extension for Extras {
    const NAME: &'static str = "extras";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Jobs)
            .on::<point::Input>(|_, input| match input.as_text() {
                Some("no") => Rewrite::Stop("not that".into()),
                Some(text) if text.starts_with("shout ") => {
                    Rewrite::Replace(text.to_uppercase().into())
                }
                _ => Rewrite::Pass,
            })
            .on::<point::PreTool>(|_, call| {
                let mut input = call.input.parse::<Value>().unwrap();
                if call.tool != "bash" || input["background"] == true {
                    return Rewrite::Pass;
                }
                input["background"] = true.into();
                Rewrite::Replace(Json::encode(&input).unwrap())
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
        invoke: None,
        tool: "jobs".into(),
        input: Json::encode(&json!({ "args": "all of them" })).unwrap(),
        id: None,
        card: None,
        by: Audience::User,
    };
    let ran = Body::ToolResult {
        call: CallId::new(2).unwrap(),
        outcome: Outcome::Done {
            output: Json::encode("all of them in 2").unwrap(),
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
                call,
                outcome: Outcome::Interrupted { .. },
                ..
            } if call.get() == 2
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
    let fixture = Fixture::new(ext::LOOP, echoing(&Policy::default()), openai(&script));
    let session = fixture.start(&[]);
    session.submit("hi").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let Body::Context { text, .. } = &log[1].body else {
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
    assert_eq!(
        tools,
        [
            "bash",
            "edit",
            "task",
            "fork_task",
            "background_task",
            "sleep"
        ]
    );
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
        output.parse::<Value>().unwrap()["output"]
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
        result.is_some_and(|output| output.parse::<Value>().unwrap()["aborted"] == true),
        "{log:#?}"
    );
    assert!(matches!(
        log.last().map(|e| &e.body),
        Some(Body::Interrupt { .. })
    ));
}

#[test]
fn replayed_process_numbers_cannot_signal_a_live_unrelated_lease() {
    let registry = echoing(&Policy::default()).with(&Extras).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let mut command =
        std::process::Command::new(sys::find_bash(std::env::var("PATH").ok().as_deref()).unwrap());
    command.args(["-c", "read -r line"]);
    command.envs(std::env::vars_os());
    let (mut child, _input, _output) = sys::spawn_stdio(&mut command).unwrap();
    let pgid = child.id();
    let log = [
        entry(1, user("$ sleep 100")),
        entry(
            2,
            Body::ToolCall {
                invoke: None,
                tool: "bash".into(),
                input: Json::encode(&json!({"command":"sleep 100"})).unwrap(),
                id: None,
                card: None,
                by: Audience::User,
            },
        ),
        entry(
            3,
            Body::ToolProcess {
                call: CallId::new(2).unwrap(),
                pgid,
                boot: sys::boot_id().unwrap().into(),
                background: true,
            },
        ),
    ];
    let session = fixture.start(&log);
    assert!(
        child.try_wait().unwrap().is_none(),
        "recovery signaled a recorded number"
    );
    session.halt();
    child.group.signal(sys::Signal::Kill).unwrap();
    child.wait().unwrap();
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
    session.wait(
        "the job ended with its session",
        Duration::from_secs(5),
        || !session.has_jobs(),
    );
    assert!(!sys::RecordedIdentity(pgid).alive());
}

/// `lookup_harbor_label`, held until released or the turn is interrupted.
struct Held(Arc<Flag>);

impl Tool for Held {
    type In = Nothing;
    type Out = String;

    const NAME: &'static str = "lookup_harbor_label";
    const DESCRIPTION: &'static str = "Return the alpha signal marker.";

    async fn call(&self, _: &Nothing, cx: &Cx) -> Result<String, String> {
        match unless(&cx.cancel, self.0.wait()).await {
            Some(()) => Ok("crimson-harbor".into()),
            None => Err("stopped".into()),
        }
    }
}

/// Offers [`Held`], released by the flag it is given.
struct HeldHarbor(Arc<Flag>);

impl Extension for HeldHarbor {
    const NAME: &'static str = "harbor";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Held(Arc::clone(&self.0)));
    }
}

fn tool_running(log: &[Entry]) -> bool {
    log.iter().any(|e| matches!(e.body, Body::ToolCall { .. }))
}

#[test]
fn a_steer_reaches_the_running_turn_before_the_model_is_next_asked() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies);
    let release = Arc::new(Flag::default());
    let tools = Registry::default()
        .with(&HeldHarbor(Arc::clone(&release)))
        .unwrap();
    let fixture = Fixture::new(ext::LOOP, tools, openai(&script));
    let session = fixture.start(&[]);
    session.submit("Call the tool, then answer.").unwrap();
    until(&session, "called the tool", tool_running);
    session.steer("Say it twice.").unwrap();
    session.submit("And then this.").unwrap();
    release.set();
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
fn an_interrupted_turn_answers_the_calls_it_leaves_and_goes_no_further() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies);
    let tools = Registry::default()
        .with(&HeldHarbor(Arc::default()))
        .unwrap();
    let fixture = Fixture::new(ext::LOOP, tools, openai(&script));
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
                    call,
                    outcome: Outcome::Failed { .. },
                    ..
                },
                Body::Interrupt { .. },
            ] if call.get() == 4
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
        invoke: None,
        tool: "bash".into(),
        input: Json::encode(&json!({ "command": "make" })).unwrap(),
        id: None,
        card: None,
        by: Audience::Model,
    };
    let progress = |text: &str| Body::Progress {
        call: CallId::new(2).unwrap(),
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
            call: CallId::new(2).unwrap(),
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
            } => Some(output.parse::<Value>().unwrap()),
            _ => None,
        })
        .collect();
    let set = |from: &str, model: &str| json!({ "model": model, "from": from });
    assert_eq!(
        results,
        [
            set("the home config.toml", "gpt-5.6"),
            set("this session", "anthropic:claude-x"),
            set("this session", "anthropic:claude-x"),
            set("the home config.toml", "gpt-5.6"),
        ]
    );
    let chosen = Body::Ext {
        ext: "settings".into(),
        state: "model".into(),
        data: Json::encode("anthropic:claude-x").unwrap(),
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

/// Counts, as a tool's own state.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Count(u64);

impl ext::Fold for Count {
    const NAME: &'static str = "count";
    type Change = u64;

    fn apply(&mut self, n: &u64) {
        self.0 += n;
    }
}

/// `/bump`: adds one to the count and answers with the count as it then
/// reads it.
struct Bump;

impl Tool for Bump {
    type In = ext::Args;
    type Out = u64;

    const NAME: &'static str = "bump";
    const DESCRIPTION: &'static str = "Counts.";
    const AUDIENCE: Audience = Audience::User;

    async fn call(&self, _: &ext::Args, cx: &Cx) -> Result<u64, String> {
        cx.update::<Count>(&1).await.map_err(|e| e.to_string())?;
        Ok(cx.state(|c: &Count| c.0))
    }
}

struct Counter;

impl Extension for Counter {
    const NAME: &'static str = "counter";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.fold::<Count>().tool(Bump);
    }
}

/// #23: a change to an extension's state is visible once committed, and
/// `update` resolves then: a call that changes its state and reads it
/// back in the same breath reads the new value, and the change is in the
/// log before the call's result.
#[test]
fn state_is_visible_only_after_commit() {
    let registry = Registry::default().with(&Counter).unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.start(&[]);
    session.submit("/bump").unwrap();
    session.submit("/bump").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    let counts: Vec<u64> = log
        .iter()
        .filter_map(|e| match &e.body {
            Body::ToolResult {
                outcome: Outcome::Done { output },
                ..
            } => output.parse().ok(),
            _ => None,
        })
        .collect();
    assert_eq!(counts, [1, 2], "{log:#?}");
    let order: Vec<&str> = log
        .iter()
        .filter_map(|e| match &e.body {
            Body::Ext { state, .. } => Some(state.as_str()),
            Body::ToolResult { .. } => Some("result"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["count", "result", "count", "result"]);
}

/// Panics on `boom`, the first time it is given it.
struct Panicky;

static BOOMED: AtomicBool = AtomicBool::new(false);

impl ext::Agent for Panicky {
    fn step(&mut self, input: ext::Input, _: &dyn ext::LoopPoints) -> ext::Actions {
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

/// Messages that queue while the actor commits are stepped together and
/// committed once: a burst of input costs one fsync, not one each.
#[test]
fn one_fsync_per_burst() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        Registry::default(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    settle(&session);
    let input = |text: &str| {
        let input = Event::Input {
            segments: text.into(),
            mode: Mode::Queue,
            origin: None,
            ticket: None,
        };
        session.send(Msg::Step(input));
    };
    let held = lock(&probe.held);
    input("first");
    while probe.waiting.load(Ordering::SeqCst) == 0 {
        thread::yield_now();
    }
    for n in 0..5 {
        input(&format!("then {n}"));
    }
    drop(held);
    settle(&session);
    let log = log_of(&session);
    session.halt();
    assert_eq!(log.len(), 1 + 2 * 6, "{log:#?}");
    // The extensions, the first input and its echo, then the burst.
    assert_eq!(*lock(&probe.batches), [1, 2, 10]);
}

/// A crash must see busy before work is logged, and settled only after
/// its result is logged.
#[test]
fn busy_is_set_before_work_is_written_and_cleared_after_its_result() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    settle(&session);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let writes = Arc::clone(&observed);
    let shared = Arc::downgrade(&fixture.0);
    *lock(&probe.observe_write) = Some(Box::new(move |entries, after| {
        for entry in entries {
            let starts = match entry.body {
                Body::Input { .. } => true,
                Body::ToolResult { .. } => false,
                _ => continue,
            };
            let shared = shared.upgrade().unwrap();
            lock(&writes).push((starts, after, lock(&shared.catalog.marks).clone()));
        }
    }));
    session.submit("$ true").unwrap();
    finished(&session, 3);
    settle(&session);
    session.halt();
    assert_eq!(
        *lock(&observed),
        [
            (true, false, vec![true]),
            (true, true, vec![true]),
            (false, false, vec![true]),
            (false, true, vec![true]),
        ]
    );
    assert_eq!(*lock(&fixture.0.catalog.marks), [true, false]);
}

/// Progress and extension state are written as they come, and made
/// durable with the next commit that has to be: a crash after any write
/// loses at most notes, never an intent, a decision or a result.
#[test]
fn a_crash_loses_at_most_notes() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session
        .submit("$ cd /; for i in 1 2 3 4; do echo $i; sleep 0.15; done")
        .unwrap();
    finished(&session, 3);
    session.submit("hi").unwrap();
    settle(&session);
    session.halt();
    let written = lock(&probe.written).clone();
    let notes = written.iter().filter(|(_, durable)| !durable).count();
    assert!(notes >= 2, "{written:#?}");
    // A crash after write `at` keeps every write up to the last durable
    // one: what it loses is what was only written after that.
    for at in 0..written.len() {
        let kept = written[..=at].iter().rposition(|(_, durable)| *durable);
        let lost = written[kept.map_or(0, |k| k + 1)..=at]
            .iter()
            .flat_map(|(entries, _)| entries);
        for e in lost {
            assert!(
                matches!(e.body, Body::Progress { .. } | Body::Ext { .. }),
                "a crash after write {at} loses {e:?}"
            );
        }
    }
    assert!(written.last().is_some_and(|(_, durable)| *durable));
}

/// A session at rest with enough log past its snapshot takes one, and
/// the next load starts from it: it reads only the log after it, with the
/// extensions' states as they were, and goes on from there.
#[test]
fn a_session_loads_from_its_snapshot_and_reads_only_the_suffix() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let header = Fixture::header();
    let session = Live::start(&header, &fixture.0, Open::Create, Owner::None).unwrap();
    session.submit("$ cd /").unwrap();
    finished(&session, 3);
    settle(&session);
    assert!(lock(&probe.snapshots).is_empty(), "too little log for one");
    let text = "x".repeat(usize::try_from(SNAPSHOT_EVERY).unwrap());
    session.submit(&text).unwrap();
    settle(&session);
    let covers = log_of(&session).len() as u64;
    assert_eq!(*lock(&probe.snapshots), [covers]);
    session.submit("after").unwrap();
    settle(&session);
    let states = lock(&session.session.state_view()).image().unwrap();
    let log = log_of(&session);
    session.halt();
    assert_eq!(*lock(&probe.snapshots), [covers], "none for a short suffix");

    probe.read.store(0, Ordering::SeqCst);
    let loaded = Live::start(&header, &fixture.0, Open::Load, Owner::None).unwrap();
    settle(&loaded);
    let read = probe.read.load(Ordering::SeqCst);
    assert_eq!(read as u64, log.len() as u64 - covers);
    assert_eq!(lock(&loaded.session.state_view()).image().unwrap(), states);
    assert_eq!(log_of(&loaded), log);
    loaded.submit("again").unwrap();
    settle(&loaded);
    let more = log_of(&loaded);
    loaded.halt();
    assert_eq!(
        facts(&more[log.len()..]),
        [
            (log.len() as u64 + 1, &user("again")),
            (log.len() as u64 + 2, &echo("again")),
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
            image: None,
        }],
    };
    let bash = Body::ToolCall {
        invoke: None,
        tool: "bash".into(),
        input: Json::encode(&json!({ "command": "make" })).unwrap(),
        id: None,
        card: None,
        by: Audience::Model,
    };
    let entries = [entry(1, older), entry(2, user("$ make")), entry(3, bash)];
    let header = Fixture::header();
    let mut created = probe.store.create(&header).unwrap();
    for e in &entries {
        let e = Entry::new(e.seq, Epoch(created.writer.epoch()), e.body.clone());
        created.writer.append_raw(&[e]).unwrap();
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
            [Body::Extensions { .. }, Body::ToolResult { call, .. }] if call.get() == 3
        ),
        "{log:#?}"
    );
    assert_eq!(*lock(&probe.batches), [2]);
}

#[test]
fn in_process_recovery_preserves_the_attached_environment() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        Registry::default(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    let env = Arc::new(Environment::new(
        std::env::temp_dir(),
        [("OPENAI_API_KEY".into(), "client-test-key".into())],
        false,
    ));
    session.set_env(Arc::clone(&env)).unwrap();
    session.barrier();
    probe.failing.store(true, Ordering::SeqCst);
    assert!(session.submit("lost").is_err());
    session.barrier();
    probe.failing.store(false, Ordering::SeqCst);
    let preserved = lock(&session.session.env)
        .as_ref()
        .is_some_and(|live| Arc::ptr_eq(live, &env));
    let log = log_of(&session);
    session.halt();
    assert!(
        preserved,
        "in-process recovery must retain the client's live environment"
    );
    assert!(
        log.iter()
            .any(|e| matches!(&e.body, Body::Environment(logged) if logged == &env.redacted()))
    );
}

struct MemoryGenerations {
    registry: Arc<Registry>,
}
impl ext::host::Generations for MemoryGenerations {
    fn snapshot(&self) -> ext::host::GenerationSnapshot {
        ext::host::GenerationSnapshot {
            id: 1,
            registry: Arc::clone(&self.registry),
        }
    }
    fn restore(self: Arc<Self>, installed: &[Installed]) -> Result<Arc<Registry>, ext::Refused> {
        if self.registry.installed() != installed {
            return Err(ext::Refused("generation differs".into()));
        }
        Ok(self.registry.clone())
    }
}
struct MemorySource(Arc<MemoryGenerations>);
impl ext::host::Sources for MemorySource {
    fn open(
        &self,
        _: &Path,
        _: ext::host::Spawn,
        _: ext::host::Prepare,
    ) -> Result<Arc<dyn ext::host::Generations>, ext::Refused> {
        Ok(self.0.clone())
    }
}
impl Extension for MemorySource {
    const NAME: &'static str = "source";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn ext::host::Sources>(Arc::new(Self(self.0.clone())));
    }
}

#[test]
fn unchanged_generations_never_prepare_or_read_the_log_on_bursts() {
    let source = Arc::new(MemoryGenerations {
        registry: Arc::new(Registry::default().with(&Agents).unwrap()),
    });
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default()
            .with(&MemorySource(source.clone()))
            .unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.start(&[]);
    session.barrier();
    let hops = fixture.hops();
    for _ in 0..10 {
        session.send(Msg::Step(Event::Note {
            call: id(999),
            note: Note::Progress("ignored".into()),
        }));
        session.barrier();
    }
    assert_eq!(
        fixture.hops(),
        hops,
        "ordinary bursts cannot dispatch blocking preparation"
    );
    session.halt();
}

#[test]
fn every_recorded_prefix_is_either_copied_exactly_or_refused_without_a_log() {
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&replies);
    let registry = Registry::default()
        .with(&Harbor)
        .unwrap()
        .with(&ext_fork::Fork)
        .unwrap();
    let fixture = Fixture::new(ext::LOOP, registry, openai(&script));
    let session = fixture.create();
    session.submit("Call the tool, then answer.").unwrap();
    settle(&session);
    let log = log_of(&session);
    let header = fixture
        .0
        .store
        .headers()
        .unwrap()
        .into_iter()
        .find(|h| h.id == session.id)
        .unwrap();
    let mut copied = 0;
    for (index, entry) in log.iter().enumerate() {
        let before = fixture.0.store.headers().unwrap().len();
        match copy_history(&header, entry.seq, SessionId::new(), &fixture.0) {
            Ok(fork) => {
                let imported = fixture.0.store.entries(fork.id).unwrap();
                assert_eq!(&imported[..=index], &log[..=index]);
                let mut expected = SessionView::default();
                let mut actual = SessionView::default();
                for entry in &log[..=index] {
                    expected.fold(&DaemonMsg::Entry(entry.clone()));
                }
                for entry in &imported[..=index] {
                    actual.fold(&DaemonMsg::Entry(entry.clone()));
                }
                assert_eq!(expected.state_hash(), actual.state_hash());
                copied += 1;
            }
            Err(error) => {
                assert_eq!(error, protocol::ForkError::Unfinished);
                assert_eq!(fixture.0.store.headers().unwrap().len(), before);
            }
        }
    }
    assert!(copied >= 2);
    assert_eq!(fixture.0.store.entries(session.id).unwrap(), log);
    session.halt();
}

#[test]
fn recorded_dynamic_history_is_refused_without_its_loader() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default().with(&ext_fork::Fork).unwrap(),
        openai(&Script::new(&[])),
    );
    let header = Fixture::header();
    let mut installed = fixture.0.registry.get().installed();
    installed.push(Installed {
        name: "counter".into(),
        version: 1,
        image: Some(
            Json::encode(&(
                "a.fold{name='count',initial=0,apply=function(s,c) return s+c end}",
                Vec::<String>::new(),
                4_194_304_usize,
                100_000_u64,
                Duration::from_millis(25),
            ))
            .unwrap(),
        ),
    });
    let entries = vec![
        entry(1, Body::Extensions { installed }),
        entry(
            2,
            Body::Ext {
                ext: "counter".into(),
                state: "count".into(),
                data: Json::encode(&7).unwrap(),
            },
        ),
        entry(
            3,
            Body::Ext {
                ext: "fork".into(),
                state: "parent".into(),
                data: Json::encode(&protocol::ForkParent {
                    session: SessionId::new(),
                    seq: Seq::new(2).unwrap(),
                })
                .unwrap(),
            },
        ),
    ];
    fixture.0.store.import(&header, &entries).unwrap();
    let mut record = Record::new(header.clone());
    record.entries = entries.len() as u64;
    fixture.0.catalog.record(record);

    assert!(matches!(
        copy_history(&header, Seq::new(3).unwrap(), SessionId::new(), &fixture.0),
        Err(protocol::ForkError::UnavailableGeneration)
    ));
    let live = Live::start(&header, &fixture.0, Open::Load, Owner::None).unwrap();
    settle(&live);
    assert_eq!(live.summary().read_only, Some(ReadOnly::Extensions));
    assert!(
        live.submit("must not replace the recorded generation")
            .is_err()
    );
    live.halt();
    assert_eq!(fixture.0.store.entries(header.id).unwrap(), entries);
}

#[test]
fn validated_prefixes_reject_sequence_gaps_and_unfinished_work_by_type() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default().with(&ext_fork::Fork).unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session.submit("complete").unwrap();
    settle(&session);
    let log = log_of(&session);
    let registry = fixture.0.registry.get();
    let policy = registry.service::<dyn ext::host::HistoryPolicy>().unwrap();
    let binding = bind(&registry, &session.session);
    let mut gap = log.clone();
    gap.remove(0);
    let boundary = log.last().unwrap().seq;
    assert!(matches!(
        ext::host::ValidatedPrefix::prepare(policy, binding.clone(), gap, session.id, boundary),
        Err(protocol::ForkError::InvalidSequence)
    ));
    let pending: Vec<_> = log
        .iter()
        .take_while(|entry| !matches!(entry.body, Body::Assistant { .. }))
        .cloned()
        .collect();
    let boundary = pending.last().unwrap().seq;
    assert!(matches!(
        ext::host::ValidatedPrefix::prepare(policy, binding, pending, session.id, boundary),
        Err(protocol::ForkError::Unfinished)
    ));
    session.halt();
}

#[test]
fn loading_cannot_wait_on_its_own_exhausted_pool() {
    let home = std::env::temp_dir().join(format!("a-c09-loading-{}", SessionId::new()));
    std::fs::create_dir_all(&home).unwrap();
    let store = Arc::new(crate::store::Jsonl::open(home.join("sessions")).unwrap());
    let mut fixture = Fixture::on(
        store.clone(),
        ext_fixtures::ECHO,
        Registry::default(),
        with_home(&Script::new(&[]), &home.join("config.toml")),
    );
    let mut config = rt::Config::for_cores(1);
    config.storage.workers = 1;
    let runtime = Arc::new(Counting(
        rt::Threads::configured(config),
        AtomicUsize::new(0),
    ));
    fixture = fixture.configure(|shared| shared.rt = runtime.clone());
    fixture.1 = runtime.clone();
    let header = Fixture::header();
    let mut log = store.create(&header).unwrap();
    log.append(echo("persisted")).unwrap();
    drop(log);
    let shared = Arc::clone(&fixture.0);
    let loaded = rt::unblock(&*runtime, move || {
        Live::start(&header, &shared, Open::Load, Owner::None)
    });
    let live = rt::block_on(loaded).unwrap();
    live.stop();
    rt::block_on(live.ended.wait());
    drop(live);
    runtime.0.shutdown();
    drop(fixture);
    drop(store);
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn calls_share_transport_pools_but_not_origin_authority() {
    let home = gpt_home();
    let fixture = Fixture::new(
        "echo",
        Registry::default(),
        with_home(&Script::new(&[]), &home),
    );
    let registry = fixture.0.registry.get();
    let (tx, _rx) = channel::unbounded();
    let session = Session::new(&Fixture::header(), &fixture.0, Arc::new(tx), &registry);
    let old_origin = session.origin(CallId::new(1).unwrap(), 0).unwrap();
    let old = session.cx(old_origin.clone(), &registry);
    let current_origin = session.origin(CallId::new(2).unwrap(), 0).unwrap();
    let current = session.cx(current_origin.clone(), &registry);
    assert!(std::ptr::eq(
        old.service::<dyn rt::Runtime>().unwrap(),
        current.service::<dyn rt::Runtime>().unwrap()
    ));
    for policy in [
        transport::RedirectPolicy::Follow,
        transport::RedirectPolicy::Refuse,
    ] {
        assert!(Arc::ptr_eq(
            &old.service::<dyn transport::HttpClients>()
                .unwrap()
                .client(policy),
            &current
                .service::<dyn transport::HttpClients>()
                .unwrap()
                .client(policy),
        ));
    }
    session.cancelled(old.call);
    assert!(!old_origin.current(&session));
    assert!(current_origin.current(&session));
    let side = ext::Conversation::turn::<Agents>("authority", "")
        .per_call()
        .key("initial")
        .request(old.call);
    assert!(rt::block_on(old.conversations().unwrap().turn(&current, side)).is_err());
    std::fs::remove_file(home).unwrap();
}

#[test]
fn child_snapshot_resume_equals_full_log_resume() {
    let probe = Probe::default();
    let fixture = Fixture::on(
        Arc::new(probe.clone()),
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let mut header = Fixture::header();
    header.parent = Some(Parent {
        session: SessionId::new(),
        call: 1,
    });
    let session = Live::start(&header, &fixture.0, Open::Create, Owner::None).unwrap();
    session
        .submit(&"x".repeat(usize::try_from(SNAPSHOT_EVERY).unwrap()))
        .unwrap();
    settle(&session);
    let covers = log_of(&session).len() as u64;
    assert_eq!(*lock(&probe.snapshots), [covers]);
    let registry = fixture.0.registry.get();
    let binding = bind(&registry, &session.session) as Arc<dyn Binding>;
    for suffix in [false, true] {
        if suffix {
            session.submit("suffix").unwrap();
            settle(&session);
        }
        let entries = log_of(&session);
        let (loaded, mut restored) =
            Folding::open(&header, &fixture.0, Open::Load, &binding, &registry).unwrap();
        assert!(restored.snapshot.is_some());
        assert_eq!(
            loaded.read,
            Encoded::of(&entries[usize::try_from(covers).unwrap()..])
                .unwrap()
                .lines()
                .map(|line| line.len() as u64 + 1)
                .sum::<u64>()
        );
        let mut full = Folding::new(&binding, &registry, &fixture.0);
        for e in &entries {
            full.fold(e);
        }
        assert_eq!(restored.core.ledger(), full.core.ledger());
        let load = Load {
            owner: Owner::Present,
            ..Load::default()
        };
        let expected = full.core.resumed(&load);
        assert!(
            expected
                .immediate
                .iter()
                .chain(expected.admitted.iter().map(|g| &g.work))
                .any(|w| matches!(
                    w,
                    kernel::Work::Report {
                        outcome: Outcome::Done { .. }
                    }
                ))
        );
        assert_eq!(restored.core.resumed(&load), expected);
    }
    session.halt();
}

#[test]
fn corrupt_or_mismatched_checkpoints_fall_back_to_the_log() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        echoing(&Policy::default()),
        openai(&Script::new(&[])),
    );
    let header = Fixture::header();
    let session = Live::start(&header, &fixture.0, Open::Create, Owner::None).unwrap();
    session.submit("result").unwrap();
    settle(&session);
    session.halt();
    let entries = log_of(&session);
    let registry = fixture.0.registry.get();
    let binding = bind(&registry, &session.session) as Arc<dyn Binding>;
    let mut full = Folding::new(&binding, &registry, &fixture.0);
    for e in &entries {
        full.fold(e);
    }
    let valid = serde_json::to_value(RecoveryImage {
        build: fixture.0.build.clone(),
        extensions: registry.installed(),
        has_input: true,
        ledger: full.core.ledger().clone(),
        states: full.states.image().unwrap(),
        invocations: full.invocations.clone(),
    })
    .unwrap();
    for fault in [
        "corrupt",
        "build",
        "extensions",
        "sequence",
        "states",
        "ledger",
    ] {
        let mut image = valid.clone();
        match fault {
            "corrupt" => image = json!({}),
            "build" => image["build"] = json!("other"),
            "extensions" => image["extensions"] = json!([]),
            "sequence" => image["ledger"]["seq"] = json!(0),
            "states" => image["states"] = json!({}),
            "ledger" => {
                image["ledger"].as_object_mut().unwrap().remove("open");
            }
            _ => unreachable!(),
        }
        let mut loaded = fixture
            .0
            .store
            .load(header.id, &mut |_| false, &mut |_| {})
            .unwrap();
        loaded
            .writer
            .snapshot(&Snapshot {
                covers: entries.len() as u64,
                image: Json::encode(&image).unwrap(),
            })
            .unwrap();
        let (loaded, restored) =
            Folding::open(&header, &fixture.0, Open::Load, &binding, &registry).unwrap();
        assert!(restored.snapshot.is_none(), "{fault}");
        assert_eq!(
            loaded.read,
            Encoded::of(&entries)
                .unwrap()
                .lines()
                .map(|line| line.len() as u64 + 1)
                .sum::<u64>(),
            "{fault}"
        );
        assert_eq!(restored.core.ledger(), full.core.ledger(), "{fault}");
        assert_eq!(
            restored.states.image().unwrap(),
            full.states.image().unwrap(),
            "{fault}"
        );
    }
}

#[test]
fn environment_bursts_retain_one_payload_and_one_mailbox_marker() {
    let home = gpt_home();
    let fixture = Fixture::new(
        "echo",
        Registry::default(),
        with_home(&Script::new(&[]), &home),
    );
    let registry = fixture.0.registry.get();
    let header = Fixture::header();
    let loaded = fixture.0.store.create(&header).unwrap();
    let (tx, rx) = channel::unbounded();
    let live = Live {
        session: Session::new(&header, &fixture.0, Arc::new(tx), &registry),
        id: header.id,
        workspace: header.workspace.clone(),
        parent: None,
        frames: Arc::new(FrameLog::new(loaded.lines, 0)),
        followers: Mutex::default(),
        busy: AtomicBool::new(false),
        has_input: AtomicBool::new(false),
        attached: AtomicUsize::new(0),
        last_active: Mutex::new(Instant::now()),
        summary: Mutex::new(summary(&header, &Ledger::default())),
        ended: Flag::default(),
    };
    let mut previous = Weak::new();
    for n in 0..1000 {
        let env = Arc::new(Environment::new(n.to_string().into(), Vec::new(), false));
        live.set_env(Arc::clone(&env)).unwrap();
        assert!(previous.upgrade().is_none());
        previous = Arc::downgrade(&env);
    }
    let oversized = Environment::new(
        "oversized".into(),
        [(
            "DATA".into(),
            "x".repeat(crate::outbox::BUDGET.bytes).into(),
        )],
        false,
    );
    assert_eq!(
        live.set_env(oversized).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(matches!(rx.try_recv(), Some(Msg::Env)));
    assert!(rx.try_recv().is_none());
    let latest = lock(&live.session.pending_env).take().unwrap();
    assert_eq!(latest.cwd, PathBuf::from("999"));
    live.set_env(Environment::new("next".into(), Vec::new(), false))
        .unwrap();
    assert!(matches!(rx.try_recv(), Some(Msg::Env)));
    assert!(rx.try_recv().is_none());
    drop(live);
    drop(fixture);
    std::fs::remove_file(home).unwrap();
}
