//! Concurrent file-backed load and snapshot measurements; run instructions
//! and results are in docs/perf/runtime-demand.md. No network provider is contacted.

use super::*;
use crate::outbox::Outbox;
use std::sync::Arc;

#[test]
#[ignore = "release-only sizing measurement; see docs/perf/runtime-demand.md"]
fn concurrent_session_sizing() {
    let (root, home) = home("runtime-sizing");
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let text =
        "A realistic assistant response with code, punctuation, and whitespace.\n".repeat(15);
    let mut headers = Vec::new();
    for _ in 0..24 {
        let header = Header {
            id: SessionId::new(),
            workspace: root.join("workspace"),
            parent: None,
        };
        let mut loaded = store.create(&header).unwrap();
        let mut entries: Vec<_> = (1..=1200)
            .map(|seq| {
                Entry::new(
                    Seq::new(seq).unwrap(),
                    Epoch(0),
                    Body::Assistant { text: text.clone() },
                )
            })
            .collect();
        loaded
            .writer
            .append(&mut entries, crate::store::AppendMode::Order)
            .unwrap();
        loaded.writer.flush_inline().unwrap();
        headers.push(header);
    }
    drop(store);
    let cpu_mib: usize = std::env::var("A_BENCH_CPU_MIB").map_or(64, |n| n.parse().unwrap());
    // Interleave configurations to reduce systematic cache/thermal bias.
    for round in 0..6 {
        for workers in if round % 2 == 0 { [2, 4, 7] } else { [7, 4, 2] } {
            for lane in ["cpu", "storage"] {
                let mut config = rt::Config::for_cores(8);
                if lane == "cpu" {
                    config.cpu.workers = workers;
                    config.cpu.bytes = cpu_mib << 20;
                } else {
                    config.storage.workers = workers;
                }
                let runtime = Arc::new(rt::Threads::configured(config));
                let daemon = Daemon::load_on(&home, &echo(), &runtime, Vec::new()).unwrap();
                let mut held = Vec::new();
                if lane == "cpu" {
                    // Resolve without attaching a client: the actor is loaded,
                    // but its attach snapshot and shared view are still cold.
                    for header in &headers {
                        held.push(
                            daemon
                                .attach_to(&Target::Id(header.id), || root.clone())
                                .unwrap(),
                        );
                    }
                    settle(&daemon);
                }
                let start = Instant::now();
                let mut jobs = Vec::new();
                for header in &headers {
                    let daemon = Arc::clone(&daemon.0);
                    let target = Target::Id(header.id);
                    let workspace = root.clone();
                    let cpu = lane == "cpu";
                    jobs.push(rt::spawn(&*runtime, async move {
                        let attachment = daemon.resolve(&target, || workspace).await.unwrap();
                        if cpu {
                            let outbox = Outbox::new();
                            attachment.session().attach(outbox.clone(), 0).await;
                            let mut bytes = 0;
                            while let Some(frame) = outbox.try_next() {
                                bytes += frame.as_bytes().len();
                            }
                            assert!(
                                bytes > 1_000_000,
                                "snapshot must include the actual projection"
                            );
                        }
                        attachment
                    }));
                }
                for job in jobs {
                    held.push(rt::block_on(job));
                }
                let elapsed = start.elapsed();
                eprintln!(
                    "runtime lane={lane} workers={workers} round={round} elapsed_ms={:.3}",
                    elapsed.as_secs_f64() * 1000.0
                );
                drop(held);
                drop(daemon);
                runtime.shutdown();
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

/// Where a measurement's sessions put their actors when each runs on an
/// executor of its own; unset, they share the daemon's workers.
pub(crate) static ACTORS: std::sync::OnceLock<fn(rt::BoxFuture<'static, ()>)> =
    std::sync::OnceLock::new();

thread_local! {
    /// The session runtime whose task this thread is polling, if any: what
    /// that task spawns stays on it.
    static HOMED: std::cell::RefCell<Option<std::sync::Weak<rt::Threads>>> =
        const { std::cell::RefCell::new(None) };
}

/// The daemon's runtime when sessions run on their own threads: a session
/// actor gets a one-worker executor, and every task spawned while one of
/// its tasks is polled joins it. Blocking lanes, flushes and the reactor
/// stay shared, as each session would share them in one daemon.
struct Split(Arc<rt::Threads>);

impl rt::Runtime for Split {
    fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
        match HOMED.with_borrow(|home| home.as_ref().and_then(std::sync::Weak::upgrade)) {
            Some(own) => own.spawn(homed(Arc::downgrade(&own), task)),
            None => self.0.spawn(task),
        }
    }

    fn spawn_blocking(
        &self,
        class: rt::BlockingClass,
        bytes: usize,
        work: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, std::io::Result<()>> {
        self.0.spawn_blocking(class, bytes, work)
    }

    fn spawn_flush(
        &self,
        work: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, std::io::Result<()>> {
        self.0.spawn_flush(work)
    }

    fn reactor(&self) -> rt::Reactor {
        self.0.reactor()
    }
}

/// `task`, marking its thread as `home`'s while it is polled.
fn homed(
    home: std::sync::Weak<rt::Threads>,
    mut task: rt::BoxFuture<'static, ()>,
) -> rt::BoxFuture<'static, ()> {
    Box::pin(std::future::poll_fn(move |cx| {
        let outer = HOMED.replace(Some(home.clone()));
        let polled = task.as_mut().poll(cx);
        HOMED.set(outer);
        polled
    }))
}

/// A session actor on a one-worker runtime of its own, which ends with it:
/// the actor holds the only strong handle.
fn on_own_thread(actor: rt::BoxFuture<'static, ()>) {
    let mut config = rt::Config::for_cores(1);
    config.workers = 1;
    let own = Arc::new(rt::Threads::configured(config));
    let held = Arc::clone(&own);
    rt::Runtime::spawn(
        &*own,
        homed(
            Arc::downgrade(&own),
            Box::pin(async move {
                actor.await;
                drop(held);
            }),
        ),
    );
}

/// The recorded tool's input: nothing, under the schema the cassette offers it with.
#[derive(serde::Deserialize)]
struct Nothing {}

impl ext::Schema for Nothing {
    fn schema(out: &mut String) {
        out.push_str(r#"{"type":"object","properties":{},"required":[]}"#);
    }
}

/// The cassette's tool, answered by one real call of the shell extension.
struct Label;

impl ext::Tool for Label {
    type In = Nothing;
    type Out = String;

    const NAME: &'static str = "lookup_harbor_label";
    const DESCRIPTION: &'static str = "Return the alpha signal marker.";

    async fn call(&self, _: &Nothing, cx: &ext::Cx) -> Result<String, String> {
        let shell = sys::shell::ExecutionFacts::current().tool();
        let input = Json::encode(&json!({"command": "echo crimson-harbor"})).unwrap();
        match cx.invoke("label", shell, &input).await.outcome {
            Outcome::Done { .. } => Ok("crimson-harbor".to_owned()),
            outcome => Err(format!("the shell call ended {outcome:?}")),
        }
    }

    fn to_model(label: &Json) -> llm::Output {
        llm::Output::Text(label.parse().unwrap())
    }
}

struct Labels;

impl ext::Extension for Labels {
    const NAME: &'static str = "labels";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut ext::Registrar) {
        r.tool(Label);
    }
}

/// The SSE events of each reply recorded in `path`, in order.
fn cassette_events(path: &str) -> Vec<Vec<String>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/rig/cassettes")
        .join(path);
    let text = std::fs::read_to_string(path).unwrap();
    serde_yaml_ng::Deserializer::from_str(&text)
        .map(|doc| {
            let doc = <serde_yaml_ng::Value as serde::Deserialize>::deserialize(doc).unwrap();
            let body = doc["then"]["body"].as_str().unwrap();
            body.split_inclusive("\n\n").map(str::to_owned).collect()
        })
        .collect()
}

/// When the provider wrote each text delta of a session's turns.
type Written = Arc<std::sync::Mutex<std::collections::HashMap<usize, Vec<Delta>>>>;

/// A text delta of a session's turn, and when it was written or read.
struct Delta {
    turn: usize,
    text: String,
    at: Instant,
}

/// A provider on a local port answering every request with the cassette's
/// next reply for its session, one SSE event every `pace`, and noting when
/// each text delta was written. A session is the `bench <n> ` its first
/// message names.
fn paced_provider(replies: Vec<Vec<String>>, pace: Duration) -> (u16, Written) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let written = Written::default();
    let (replies, log) = (Arc::new(replies), Arc::clone(&written));
    // Requests so far, by session.
    let asked = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
        usize,
        usize,
    >::new()));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            stream.set_nodelay(true).unwrap();
            let (replies, log, asked) =
                (Arc::clone(&replies), Arc::clone(&log), Arc::clone(&asked));
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap();
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let body = String::from_utf8(body).unwrap();
                    let (_, named) = body.split_once("bench ").unwrap();
                    let session: usize = named.split_once(' ').unwrap().0.parse().unwrap();
                    let n = {
                        let mut asked = asked.lock().unwrap();
                        let n = asked.entry(session).or_default();
                        *n += 1;
                        *n - 1
                    };
                    let events = &replies[n % replies.len()];
                    let length: usize = events.iter().map(String::len).sum();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {length}\r\n\r\n"
                    );
                    if stream.write_all(head.as_bytes()).is_err() {
                        return;
                    }
                    for event in events {
                        std::thread::sleep(pace);
                        if stream.write_all(event.as_bytes()).is_err() {
                            return;
                        }
                        let at = Instant::now();
                        if event.starts_with("event: response.output_text.delta") {
                            let data = event.split_once("data: ").unwrap().1;
                            let data: serde_json::Value =
                                serde_json::from_str(data.trim()).unwrap();
                            let text = data["delta"].as_str().unwrap().to_owned();
                            let turn = n / replies.len();
                            let delta = Delta { turn, text, at };
                            log.lock().unwrap().entry(session).or_default().push(delta);
                        }
                    }
                }
            });
        }
    });
    (port, written)
}

/// This process's resident set (KiB), CPU seconds and threads, as the
/// hosted harness's native counters report them.
fn usage() -> serde_json::Value {
    let python = std::env::var("A_PERF_PYTHON").unwrap_or_else(|_| "python3".into());
    let script = std::env::var("A_PERF_PROCESS").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/perf/process.py").into()
    });
    let out = std::process::Command::new(python)
        .args([script, std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// What one client saw of its session's turns.
struct Seen {
    deltas: Vec<Delta>,
    turns: Vec<Duration>,
    /// Model calls that failed: each ends its turn early.
    failed: usize,
}

/// One measured sample's shape, from `A_BENCH_*`.
struct Load {
    sessions: usize,
    turns: usize,
    pace: Duration,
}

/// `A_BENCH_SESSIONS` sessions at once, each attached to by a client of
/// its own over the daemon's endpoint and running `A_BENCH_TURNS` turns of
/// the default loop: a recorded reply streamed at `A_BENCH_PACE_MS` per
/// event, its tool call answered by a real shell call, and the recorded
/// answer streamed. `A_BENCH_RUNTIME` changes the daemon's runtime by
/// comma-separated parts: `workers=<n>` async workers, `network=<n>`
/// network lane workers (and queue), `threads` for each session on an
/// executor of its own. Prints one JSON line; docs/perf/runtime-demand.md.
#[test]
#[ignore = "release-only sizing measurement; see docs/perf/runtime-demand.md"]
fn concurrent_active_sessions() {
    let var =
        |name: &str, default: u64| std::env::var(name).map_or(default, |n| n.parse().unwrap());
    let load = Load {
        sessions: usize::try_from(var("A_BENCH_SESSIONS", 8)).unwrap(),
        turns: usize::try_from(var("A_BENCH_TURNS", 3)).unwrap(),
        pace: Duration::from_millis(var("A_BENCH_PACE_MS", 10)),
    };
    let variant = std::env::var("A_BENCH_RUNTIME").unwrap_or_else(|_| "workers=4".into());
    let before = usage();
    let (root, home) = home("active-sessions");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (port, written) = paced_provider(
        cassette_events("openai/streaming_grammar/tool_then_followup_text.yaml"),
        load.pace,
    );
    let (daemon, runtime, workers) = serve(&home, port, &variant);
    // Phases on stderr, so a stalled sample says where it stalled.
    eprintln!("bench: serving");
    let start = Arc::new(std::sync::Barrier::new(load.sessions + 1));
    let (done, finished) = std::sync::mpsc::channel();
    let (release, hold) = std::sync::mpsc::channel::<()>();
    let hold = Arc::new(std::sync::Mutex::new(hold));
    let load = Arc::new(load);
    let clients: Vec<_> = (0..load.sessions)
        .map(|n| {
            let (endpoint, workspace, load) =
                (home.endpoint(), workspace.clone(), Arc::clone(&load));
            let (start, done, hold) = (Arc::clone(&start), done.clone(), Arc::clone(&hold));
            std::thread::spawn(move || {
                let mut client = Client::attach(&endpoint, workspace);
                start.wait();
                done.send((n, client.run(n, &load))).unwrap();
                // Stay attached, idle, until the measurement ends.
                let _ = hold.lock().unwrap().recv();
            })
        })
        .collect();
    // A client that panics hangs up its sender, failing the receive below.
    drop(done);
    start.wait();
    eprintln!("bench: attached");
    let active = usage();
    let started = Instant::now();
    let seen: Vec<(usize, Seen)> = (0..load.sessions)
        .map(|_| finished.recv().unwrap())
        .collect();
    let wall = started.elapsed();
    let ended = usage();
    eprintln!("bench: turns done");
    let turns: Vec<_> = (seen.iter())
        .flat_map(|(_, seen)| seen.turns.iter().map(|t| t.as_secs_f64() * 1000.0))
        .collect();
    let (deltas, dropped_at) = deliveries(&seen, &written.lock().unwrap());
    // Blocking workers retire after ten idle seconds.
    std::thread::sleep(Duration::from_secs(12));
    let settled = usage();
    std::thread::sleep(Duration::from_secs(5));
    let idle = usage();
    let cpu = |a: &serde_json::Value, b: &serde_json::Value| {
        (b["cpu_seconds"].as_f64().unwrap() - a["cpu_seconds"].as_f64().unwrap()) * 1000.0
    };
    let switches = |a: &serde_json::Value, b: &serde_json::Value| {
        b["ctx_switches"].as_u64().unwrap() - a["ctx_switches"].as_u64().unwrap()
    };
    println!(
        "{}",
        json!({
            "runtime": variant,
            "sessions": load.sessions,
            "turns": load.turns,
            "pace_ms": var("A_BENCH_PACE_MS", 10),
            "workers": workers,
            "cores": sys::resources::effective_parallelism(),
            "delta_ms": deltas,
            "dropped_deltas": dropped_at.values().sum::<usize>(),
            "failed_calls": seen.iter().map(|(_, seen)| seen.failed).sum::<usize>(),
            "dropped_at": dropped_at,
            "turn_ms": turns,
            "active_wall_ms": wall.as_secs_f64() * 1000.0,
            "active_cpu_ms": cpu(&active, &ended),
            "active_switches": switches(&active, &ended),
            "baseline_rss_kib": before["rss_kib"],
            "idle_rss_kib": settled["rss_kib"],
            "idle_threads": settled["thread_count"],
            "idle_cpu_ms": cpu(&settled, &idle),
            "idle_switches": switches(&settled, &idle),
        })
    );
    eprintln!("bench: measured");
    drop(release);
    for client in clients {
        client.join().unwrap();
    }
    drop(daemon);
    runtime.shutdown();
    let _ = std::fs::remove_dir_all(root);
}

/// Each delta's latency, provider write to client read, in milliseconds;
/// and how many never reached their client, by their place in their reply.
/// Deltas are best effort, and a turn's texts are distinct.
fn deliveries(
    seen: &[(usize, Seen)],
    written: &std::collections::HashMap<usize, Vec<Delta>>,
) -> (Vec<f64>, std::collections::BTreeMap<usize, usize>) {
    let mut latencies = Vec::new();
    let mut dropped = std::collections::BTreeMap::new();
    for (n, seen) in seen {
        for reply in written[n].chunk_by(|a, b| a.turn == b.turn) {
            for (place, wrote) in reply.iter().enumerate() {
                let read =
                    (seen.deltas.iter()).find(|d| d.turn == wrote.turn && d.text == wrote.text);
                match read {
                    Some(read) => latencies
                        .push(read.at.saturating_duration_since(wrote.at).as_secs_f64() * 1000.0),
                    None => *dropped.entry(place).or_insert(0) += 1,
                }
            }
        }
    }
    (latencies, dropped)
}

/// A daemon on `home` serving its endpoint, its sessions' model at the
/// provider on `port`, on the runtime layout `variant` names (see
/// [`concurrent_active_sessions`]), and how
/// many async workers that runtime has.
fn serve(home: &Home, port: u16, variant: &str) -> (crate::Owner, Arc<rt::Threads>, usize) {
    std::fs::write(
        home.config_file(),
        format!(
            "model = \"gpt-5.6\"\n[env]\nOPENAI_API_KEY = \"bench\"\nOPENAI_BASE_URL = \"http://127.0.0.1:{port}/v1\"\n"
        ),
    )
    .unwrap();
    let config = Config {
        extensions: |_| {
            extensions(&ext_policy::Policy {
                mode: ext_policy::Mode::Auto,
                ..ext_policy::Policy::default()
            })?
            .with_in(ext::Tier::Pre, &Labels)
        },
        ..Config::default()
    };
    let mut threads = rt::Config::daemon();
    let mut split = false;
    for part in variant.split(',') {
        match part.split_once('=') {
            Some(("workers", n)) => threads.workers = n.parse().unwrap(),
            // About a MiB of reservation per streamed request.
            Some(("network", n)) => {
                let n = n.parse().unwrap();
                threads.network = rt::PoolLimits {
                    workers: n,
                    queue: n,
                    bytes: threads.network.bytes.max(n << 20),
                };
            }
            None if part == "threads" => {
                ACTORS.set(on_own_thread).unwrap();
                split = true;
            }
            _ => panic!("A_BENCH_RUNTIME part {part:?}"),
        }
    }
    let runtime = Arc::new(rt::Threads::configured(threads));
    let daemon = if split {
        Daemon::load_on(
            home,
            &config,
            &Arc::new(Split(Arc::clone(&runtime))),
            Vec::new(),
        )
    } else {
        Daemon::load_on(home, &config, &runtime, Vec::new())
    }
    .unwrap();
    // The locked daemon record is how a Windows client finds the pipe.
    std::fs::create_dir_all(home.daemon_pid_file().parent().unwrap()).unwrap();
    let crate::Claim::Serve(record) = crate::claim(home, "bench", None, false, 0).unwrap() else {
        panic!("the bench home's breaker tripped");
    };
    std::mem::forget(record);
    let listener: &'static sys::Listener = Box::leak(Box::new(home.bind().unwrap()));
    let life = Arc::new(crate::Lifecycle::new("bench".into(), home.runtime()));
    let serving = (Arc::clone(&daemon.0), life);
    daemon.shared.rt.spawn(Box::pin(async move {
        let (daemon, life) = serving;
        let listener = rt::Async::new(&daemon.shared.rt.reactor(), listener).unwrap();
        let _ = crate::server::accept(&listener, &daemon, &life).await;
    }));
    (daemon, runtime, threads.workers)
}

/// A client of the daemon's endpoint, attached to a new session.
struct Client {
    stream: sys::OwnedConnection,
    reader: std::io::BufReader<sys::OwnedConnection>,
}

impl Client {
    fn attach(endpoint: &sys::Endpoint, workspace: PathBuf) -> Self {
        let stream = endpoint.connect().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .unwrap();
        let reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut client = Self { stream, reader };
        client.send(&protocol::ClientMsg::Hello {
            proto: protocol::PROTO,
            build: "bench".into(),
            caps: protocol::Caps::NONE,
            client: None,
            machine: None,
        });
        assert!(matches!(client.recv(), protocol::DaemonMsg::Welcome { .. }));
        client.send(&protocol::ClientMsg::Attach {
            target: Target::New,
            workspace: Some(workspace.try_into().unwrap()),
            since: 0,
            env: None,
            decl: protocol::ClientDecl::default(),
        });
        while !matches!(client.recv(), protocol::DaemonMsg::Synced { .. }) {}
        client
    }

    /// Session `n`'s turns, each over once the session is idle after both
    /// of its model calls' results.
    fn run(&mut self, n: usize, load: &Load) -> Seen {
        // Spread the sessions' first requests over one pace.
        std::thread::sleep(
            load.pace * u32::try_from(n).unwrap() / u32::try_from(load.sessions).unwrap(),
        );
        let mut seen = Seen {
            deltas: Vec::new(),
            turns: Vec::new(),
            failed: 0,
        };
        for turn in 0..load.turns {
            let asked = Instant::now();
            self.send(&protocol::ClientMsg::Input {
                segments: format!("bench {n} turn {turn}").into(),
                mode: protocol::Mode::Queue,
                force: false,
                client_seq: Some(turn as u64 + 1),
            });
            let (mut results, mut failed) = (0, false);
            loop {
                match self.recv() {
                    protocol::DaemonMsg::Delta { text, .. } => {
                        let at = Instant::now();
                        seen.deltas.push(Delta { turn, text, at });
                    }
                    protocol::DaemonMsg::Entry(entry) => {
                        if let Body::ModelResult { outcome, .. } = entry.body {
                            results += 1;
                            failed |= matches!(outcome, Outcome::Failed { .. });
                        }
                    }
                    protocol::DaemonMsg::Activity { activity, .. } => match activity {
                        Activity::Idle if results == 2 || failed => break,
                        Activity::Idle | Activity::Running => {}
                        other => panic!("session {n} turn {turn}: {other:?}"),
                    },
                    _ => {}
                }
            }
            seen.turns.push(asked.elapsed());
            seen.failed += usize::from(failed);
        }
        seen
    }

    fn send(&mut self, msg: &protocol::ClientMsg) {
        protocol::write_frame(&mut self.stream, msg).unwrap();
    }

    fn recv(&mut self) -> protocol::DaemonMsg {
        protocol::read_frame(&mut self.reader)
            .unwrap()
            .expect("the daemon hung up")
    }
}
