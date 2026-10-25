#![cfg(feature = "threads")]
//! What one I/O event costs the async workers: a byte echoed by a task on
//! [`rt::Threads`] over loopback TCP, a blocking client timing each round
//! trip. Prints one JSON line; docs/perf/runtime-demand.md.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rt::{Async, Runtime, Threads};

#[test]
#[ignore = "release-only measurement; see docs/perf/runtime-demand.md"]
fn reactor_round_trip() {
    const TRIPS: usize = 5000;
    let rt = Arc::new(Threads::new(4));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    client.set_nodelay(true).unwrap();
    let (server, _) = listener.accept().unwrap();
    server.set_nodelay(true).unwrap();
    let server = Async::socket(&rt.reactor(), server).unwrap();
    rt.spawn(Box::pin(async move {
        let mut byte = [0];
        while server
            .read_with(|mut s| s.read(&mut byte))
            .await
            .is_ok_and(|n| n == 1)
        {
            server.write_all(&byte).await.unwrap();
        }
    }));
    let mut byte = [0];
    let before = usage();
    let mut trips: Vec<f64> = (0..TRIPS)
        .map(|n| {
            // Paced, so each trip finds the workers parked.
            std::thread::sleep(Duration::from_micros(200));
            let sent = Instant::now();
            client.write_all(&[u8::try_from(n % 256).unwrap()]).unwrap();
            client.read_exact(&mut byte).unwrap();
            sent.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    let after = usage();
    trips.sort_by(f64::total_cmp);
    let at = |q: f64| trips[((trips.len() as f64 * q) as usize).min(trips.len() - 1)];
    println!(
        "{{\"trips\":{TRIPS},\"p50_us\":{:.2},\"p90_us\":{:.2},\"p99_us\":{:.2},\"wakes\":{},\"cpu_ms\":{:.1},\"switches\":{}}}",
        at(0.5),
        at(0.9),
        at(0.99),
        rt::wakes(),
        (after.0 - before.0) * 1000.0,
        after.1 - before.1
    );
    drop(client);
    rt.shutdown();
}

/// This process's CPU seconds and context switches, through psutil
/// (scripts/perf/process.py), which reads each OS's native counters.
fn usage() -> (f64, u64) {
    let script = std::env::var("A_PERF_PROCESS").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/perf/process.py").into()
    });
    let python = std::env::var("A_PERF_PYTHON").unwrap_or_else(|_| "python3".into());
    let out = std::process::Command::new(python)
        .args([script, std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let field = |name: &str| {
        let at = text.find(&format!("\"{name}\": ")).unwrap() + name.len() + 4;
        let end = text[at..].find([',', '}']).unwrap();
        text[at..at + end].to_owned()
    };
    (
        field("cpu_seconds").parse().unwrap(),
        field("ctx_switches").parse().unwrap(),
    )
}
