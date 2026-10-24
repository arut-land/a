//! Opt-in collector probe; ordinary tests keep all diagnostics local.

use super::Native;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ROUNDS: usize = 5;
const RECORDS: usize = 500;
const BYTES: usize = 128;

fn record(token: &str, sequence: i64) -> Vec<u8> {
    let head = format!(r#"{{"native_probe":"{token}","sequence":{sequence},"padding":""#);
    let line = format!("{head}{}\"}}\n", "x".repeat(BYTES - head.len() - 3));
    assert_eq!(line.len(), BYTES);
    line.into_bytes()
}

fn elapsed(sink: &Native, line: &[u8]) -> u64 {
    let start = Instant::now();
    sink.write(std::hint::black_box(line));
    u64::try_from(start.elapsed().as_nanos()).unwrap()
}

fn await_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(Instant::now() < deadline, "collector did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "requires an explicitly enabled native collector on a hosted runner"]
fn listening_collector_cost_per_record() {
    let dir = std::path::PathBuf::from(std::env::var_os("A_NATIVE_PROBE_DIR").unwrap());
    let token = format!(
        "native_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    // This explicit probe bypasses home routing without opening daemon state.
    let native = Native::new(false);
    assert!(native.0.is_some(), "native transport is unavailable");
    let disabled = Native::new(true);
    let provider: Option<String> = {
        #[cfg(windows)]
        {
            Some(super::backend::probe_provider())
        }
        #[cfg(not(windows))]
        {
            None
        }
    };
    fs::write(
        dir.join("metadata.json"),
        serde_json::to_vec(&serde_json::json!({
            "pid": std::process::id(), "token": token, "provider": provider,
            "rounds": ROUNDS, "records_per_round": RECORDS, "input_bytes": BYTES
        }))
        .unwrap(),
    )
    .unwrap();
    await_file(&dir.join("ready"));
    #[cfg(windows)]
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !super::backend::probe_enabled() {
            assert!(Instant::now() < deadline, "ETW provider was not enabled");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !dir.join("ack").exists() {
        assert!(
            Instant::now() < deadline,
            "collector did not receive the handshake"
        );
        native.write(&record(&token, -2));
        std::thread::sleep(Duration::from_millis(50));
    }
    for _ in 0..32 {
        native.write(&record(&token, -1));
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut enabled_ns = Vec::new();
    let mut disabled_ns = Vec::new();
    for round in 0..ROUNDS {
        let (mut enabled, mut off) = (Vec::new(), Vec::new());
        for sequence in round * RECORDS..(round + 1) * RECORDS {
            let line = record(&token, i64::try_from(sequence).unwrap());
            let (on, noop) = if sequence % 2 == 0 {
                let on = elapsed(std::hint::black_box(&native), &line);
                (on, elapsed(std::hint::black_box(&disabled), &line))
            } else {
                let noop = elapsed(std::hint::black_box(&disabled), &line);
                (elapsed(std::hint::black_box(&native), &line), noop)
            };
            enabled.push(on);
            off.push(noop);
            // Collector draining and pacing are outside the timed call.
            std::thread::sleep(Duration::from_millis(10));
        }
        enabled_ns.push(enabled);
        disabled_ns.push(off);
    }
    fs::write(
        dir.join("samples.json"),
        serde_json::to_vec(&serde_json::json!({
            "enabled_ns": enabled_ns, "disabled_ns": disabled_ns
        }))
        .unwrap(),
    )
    .unwrap();
}
