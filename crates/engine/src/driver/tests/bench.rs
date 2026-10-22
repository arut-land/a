//! Measurements, not tests: run on a release build with
//! `cargo nextest run --workspace --release --run-ignored only -E 'test(/bench::/)' --no-capture`;
//! `A_BENCH_ENTRIES` sets the log's length. What they found is in
//! `docs/perf/sessions.md`.

use std::time::Instant;

use protocol::Decode as _;

use crate::store::Jsonl;

use super::*;

/// The process's resident set, in KiB.
fn rss() -> u64 {
    let out = if let Ok(python) = std::env::var("A_PERF_PYTHON") {
        std::process::Command::new(python)
            .args([
                &std::env::var("A_PERF_PROCESS").unwrap(),
                &std::process::id().to_string(),
                "rss",
            ])
            .output()
            .unwrap()
    } else {
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap()
    };
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

/// A session of the default loop, each turn a tool call and an answer from
/// a recorded reply, as long as `A_BENCH_ENTRIES` says (15 600 unset): one
/// turn run, then laid down again renumbered, as running them all would
/// send each model call the whole conversation so far.
fn long_log() -> Vec<Entry> {
    let entries: usize = std::env::var("A_BENCH_ENTRIES").map_or(15_600, |n| n.parse().unwrap());
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let tools = Registry::default().with(&Harbor).unwrap();
    let fixture = Fixture::new(ext::LOOP, tools, openai(&Script::new(&replies)));
    let session = fixture.start(&[]);
    session.submit("Call the tool, then answer.").unwrap();
    settle(&session);
    let turn = log_of(&session);
    session.halt();
    let n = turn.len() as u64;
    (0..)
        .flat_map(|k| turn.iter().map(move |e| (k * n, e)))
        .take(entries)
        .map(|(by, e)| {
            let mut e = serde_json::to_value(e).unwrap();
            for field in ["seq", "call"] {
                if let Some(seq) = e[field].as_u64() {
                    e[field] = json!(seq + by);
                }
            }
            Entry::from_json(&serde_json::to_vec(&e).unwrap()).unwrap()
        })
        .collect()
}

/// Load time and the memory a loaded session holds, its log read whole
/// and from a snapshot: the log is folded and dropped as it is read, so
/// neither grows with its length but by the index, 8 bytes an entry.
#[test]
#[ignore = "a measurement"]
fn bench_loading_a_long_log() {
    let log = long_log();
    let dir = std::env::temp_dir().join(format!("a-bench-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(Jsonl::open(&dir).unwrap());
    let header = Fixture::header();
    let mut created = store.create(&header).unwrap();
    for chunk in log.chunks(4096) {
        let epoch = Epoch(created.writer.epoch());
        let chunk: Vec<Entry> = (chunk.iter())
            .map(|e| Entry::new(e.seq, epoch, e.body.clone()))
            .collect();
        created.writer.append_raw(&chunk).unwrap();
    }
    drop(created);
    let bytes = std::fs::metadata(dir.join(format!("{}/log.jsonl", header.id)))
        .unwrap()
        .len();
    let derived = dir.join(header.id.to_string()).join("derived");
    let load = |whole: bool| {
        if whole {
            std::fs::remove_dir_all(&derived).unwrap();
            std::fs::create_dir(&derived).unwrap();
        }
        let tools = Registry::default().with(&Harbor).unwrap();
        let fixture = Fixture::on(
            Arc::clone(&store) as _,
            ext::LOOP,
            tools,
            openai(&Script::new(&[])),
        );
        let before = rss();
        let start = Instant::now();
        let session = Live::start(&header, &fixture.0, Open::Load, Owner::None).unwrap();
        settle(&session);
        let took = start.elapsed();
        let held = rss().saturating_sub(before);
        assert!(session.frames.seq() >= log.len() as u64);
        session.halt();
        (took, held)
    };
    for (name, whole) in [("whole", true), ("from its snapshot", false)] {
        // The first of each warms the page cache and, whole, leaves the
        // snapshot the next starts from.
        let runs: Vec<_> = (0..6).map(|_| load(whole)).collect();
        let took = median(runs[1..].iter().map(|r| r.0).collect());
        let held = runs[1..].iter().map(|r| r.1).max().unwrap();
        eprintln!(
            "{} entries, {} KiB, loaded {name}: {took:?} (median of 5), at most {held} KiB more resident",
            log.len(),
            bytes / 1024,
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// What each barrier setting costs a turn of the default loop that calls a
/// tool, committed to a file on disk, and the entries a second it appends.
#[test]
#[ignore = "a measurement"]
fn bench_turns_under_each_barrier_setting() {
    let turns: usize = std::env::var("A_BENCH_DRIVER_TURNS").map_or(50, |n| n.parse().unwrap());
    assert!(turns > 0);
    let turn = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    for barriers in [kernel::Barriers::Step, kernel::Barriers::Effects] {
        let dir = std::env::temp_dir().join(format!("a-bench-turns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let replies: Vec<String> = (0..turns).flat_map(|_| turn.iter().cloned()).collect();
        let tools = Registry::default().with(&Harbor).unwrap();
        let mut fixture = Fixture::on(
            Arc::new(Jsonl::open(&dir).unwrap()),
            ext::LOOP,
            tools,
            openai(&Script::new(&replies)),
        );
        fixture = fixture.configure(|shared| shared.barriers = barriers);
        let session = fixture.create();
        settle(&session);
        let start = Instant::now();
        let times: Vec<Duration> = (0..turns)
            .map(|n| {
                let start = Instant::now();
                session.submit(&format!("Call the tool. ({n})")).unwrap();
                settle(&session);
                start.elapsed()
            })
            .collect();
        let all = start.elapsed();
        let entries = log_of(&session).len();
        session.halt();
        eprintln!(
            "{barriers:?}: a turn takes {:?} (median of {turns}); {:.0} entries a second",
            median(times),
            f64::from(u32::try_from(entries).unwrap()) / all.as_secs_f64(),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Simultaneous echo turns through real actors and JSONL writers, including
/// their publication barriers, rather than isolated filesystem calls.
#[test]
#[ignore = "a measurement"]
fn bench_concurrent_session_commits() {
    const ROUNDS: usize = 30;
    for n in [1, 8, 64] {
        let dir =
            std::env::temp_dir().join(format!("a-bench-concurrent-{}-{n}", std::process::id()));
        let fixture = Fixture::on(
            Arc::new(Jsonl::open(&dir).unwrap()),
            ext_fixtures::ECHO,
            Registry::default(),
            openai(&Script::new(&[])),
        );
        let sessions: Vec<_> = (0..n).map(|_| fixture.create()).collect();
        for session in &sessions {
            settle(session);
        }
        let mut times = Vec::new();
        let mut elapsed = Duration::ZERO;
        for _ in 0..ROUNDS {
            let ready = std::sync::Barrier::new(n + 1);
            thread::scope(|scope| {
                let workers: Vec<_> = sessions
                    .iter()
                    .map(|session| {
                        let ready = &ready;
                        scope.spawn(move || {
                            ready.wait();
                            let start = Instant::now();
                            session.submit("hello").unwrap();
                            settle(session);
                            start.elapsed()
                        })
                    })
                    .collect();
                let start = Instant::now();
                ready.wait();
                for worker in workers {
                    times.push(worker.join().unwrap());
                }
                elapsed += start.elapsed();
            });
        }
        times.sort();
        eprintln!(
            "{n} sessions: {:.0} turns/s, p50 {:?}, p99 {:?} ({} turns)",
            f64::from(u32::try_from(n * ROUNDS).unwrap()) / elapsed.as_secs_f64(),
            times[times.len() / 2],
            times[(times.len() * 99 / 100).min(times.len() - 1)],
            times.len()
        );
        for session in sessions {
            session.halt();
        }
        drop(fixture);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// A large JSON value through tool encoding, durable storage and replay.
#[test]
#[ignore = "a measurement"]
fn bench_large_tool_result() {
    struct Large;
    impl Tool for Large {
        type In = Nothing;
        type Out = String;
        const NAME: &'static str = "large";
        const DESCRIPTION: &'static str = "Return a large result.";
        const REPLAY: Replay = Replay::Safe;

        fn call(&self, _: &Nothing, _: &Cx) -> Result<String, String> {
            Ok("quoted \"text\" \\ 日本語\n".repeat(40_000))
        }
    }
    struct Tools;
    impl Extension for Tools {
        const NAME: &'static str = "large";
        const VERSION: u32 = 1;
        fn contribute(&self, r: &mut Registrar) {
            r.tool(Large);
        }
    }
    let dir = std::env::temp_dir().join(format!("a-bench-json-{}", std::process::id()));
    let store = Arc::new(Jsonl::open(&dir).unwrap());
    let fixture = Fixture::on(
        Arc::clone(&store) as _,
        ext_fixtures::ECHO,
        Registry::default().with(&Tools).unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    settle(&session);
    // Longer batches reduce quantization in native process CPU counters.
    let rounds: usize = std::env::var("A_BENCH_JSON_RESULTS").map_or(20, |n| n.parse().unwrap());
    assert!(rounds > 0);
    let mut times = Vec::new();
    for _ in 0..rounds {
        let start = Instant::now();
        session.submit("call large {}").unwrap();
        settle(&session);
        times.push(start.elapsed());
    }
    let entries = log_of(&session);
    let results = entries
        .iter()
        .filter(|e| matches!(e.body, Body::ToolResult { .. }))
        .count();
    assert_eq!(results, rounds);
    eprintln!(
        "large JSON tool result: {:?} (median of {rounds}), {} entries",
        median(times),
        entries.len()
    );
    session.halt();
    drop(fixture);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
