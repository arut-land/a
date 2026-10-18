use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::*;

#[test]
fn block_on_returns_the_output() {
    assert_eq!(block_on(async { 7 }), 7);
}

#[test]
fn threads_run_spawned_blocking_and_timed_work() {
    let rt = Threads::new(2);
    let started = Instant::now();
    let out = block_on(async {
        let slept = rt.sleep(Duration::from_millis(20));
        let blocking = unblock(&rt, || 40);
        let spawned = spawn(&rt, async { 2 });
        slept.await;
        blocking.await + spawned.await
    });
    assert_eq!(out, 42);
    assert!(started.elapsed() >= Duration::from_millis(20));
}

#[test]
fn blocking_pool_runs_work_concurrently() {
    let rt = Threads::new(1);
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let jobs: Vec<_> = (0..4)
        .map(|_| {
            let (running, peak) = (Arc::clone(&running), Arc::clone(&peak));
            unblock(&rt, move || {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                running.fetch_sub(1, Ordering::SeqCst);
            })
        })
        .collect();
    block_on(async {
        for job in jobs {
            job.await;
        }
    });
    assert_eq!(peak.load(Ordering::SeqCst), 4);
}

#[test]
fn a_bounded_channel_streams_and_ends() {
    let rt = Threads::new(1);
    let (tx, rx) = channel::bounded(2);
    rt.spawn_blocking(Box::new(move || {
        for n in 0..10 {
            tx.send(n).unwrap();
        }
    }));
    let got = block_on(async {
        let mut got = Vec::new();
        while let Some(n) = std::future::poll_fn(|cx| rx.poll_recv(cx)).await {
            got.push(n);
        }
        got
    });
    assert_eq!(got, (0..10).collect::<Vec<_>>());
}

#[test]
fn a_dropped_receiver_releases_a_waiting_sender() {
    let (tx, rx) = channel::bounded(1);
    tx.send(1).unwrap();
    let waiting = std::thread::spawn(move || tx.send(2));
    std::thread::sleep(Duration::from_millis(20));
    drop(rx);
    assert_eq!(waiting.join().unwrap(), Err(2));
}
