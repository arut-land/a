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
        while let Some(n) = rx.recv().await {
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

#[test]
fn sleeps_wake_in_deadline_order() {
    let rt = Threads::new(1);
    let order = Arc::new(Mutex::new(Vec::new()));
    let jobs: Vec<_> = [30, 10, 20, 10]
        .into_iter()
        .enumerate()
        .map(|(n, ms)| {
            let (order, sleep) = (Arc::clone(&order), rt.sleep(Duration::from_millis(ms)));
            spawn(&rt, async move {
                sleep.await;
                order.lock().unwrap().push(n);
            })
        })
        .collect();
    block_on(async {
        for job in jobs {
            job.await;
        }
    });
    assert_eq!(*order.lock().unwrap(), [1, 3, 2, 0]);
}

#[test]
fn an_unbounded_sender_never_waits() {
    let (tx, rx) = channel::unbounded();
    for n in 0..1000 {
        tx.send(n).unwrap();
    }
    drop(tx);
    let got = block_on(async {
        let mut got = 0;
        while rx.recv().await.is_some() {
            got += 1;
        }
        got
    });
    assert_eq!(got, 1000);
}

// Each test runs in a process of its own (nextest), so the reactor's
// wake count is this test's alone.
#[test]
fn an_idle_reactor_never_wakes() {
    block_on(Timer::after(Duration::from_millis(5)));
    // A dropped timer is forgotten, not woken for.
    let pending = std::pin::pin!(Timer::after(Duration::from_millis(50)))
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
    assert!(pending.is_pending());
    // Setting it woke the reactor to wait for it.
    std::thread::sleep(Duration::from_millis(10));
    let before = wakes();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(wakes(), before);
}

#[test]
fn a_timer_wakes_at_its_deadline_not_before() {
    let started = Instant::now();
    block_on(Timer::after(Duration::from_millis(30)));
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(30), "{took:?}");
    assert!(took < Duration::from_millis(500), "{took:?}");
    // A deadline earlier than the one the reactor waits for still fires
    // on time.
    let rt = Threads::new(1);
    let late = spawn(&rt, Timer::after(Duration::from_secs(30)));
    std::thread::sleep(Duration::from_millis(10));
    let started = Instant::now();
    block_on(Timer::after(Duration::from_millis(10)));
    assert!(started.elapsed() < Duration::from_millis(500));
    drop(late);
}

#[cfg(unix)]
#[test]
fn a_registered_pipe_reads_when_written_without_a_thread_per_read() {
    use std::io::{Read, Write};
    let (reader, mut writer) = std::io::pipe().unwrap();
    let reader = Async::new(reader).unwrap();
    let rt = Threads::new(1);
    let wrote = unblock(&rt, move || {
        for chunk in [&b"one"[..], b"two"] {
            std::thread::sleep(Duration::from_millis(20));
            writer.write_all(chunk).unwrap();
        }
    });
    let got = block_on(async {
        let mut got = Vec::new();
        let mut buf = [0; 16];
        loop {
            let n = reader.read_with(|mut r| r.read(&mut buf)).await.unwrap();
            if n == 0 {
                return got;
            }
            got.extend_from_slice(&buf[..n]);
        }
    });
    block_on(wrote);
    assert_eq!(got, b"onetwo");
}
