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
        loaded.writer.append(&mut entries, true).unwrap();
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
                let daemon = Daemon::load_on(&home, &echo(), &runtime).unwrap();
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
