use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Waker};
use std::time::{Duration, Instant};

use super::*;

#[test]
fn block_on_returns_the_output() {
    assert_eq!(block_on(async { 7 }), 7);
}

#[test]
fn running_maintenance_never_occupies_the_foreground_cpu_worker() {
    let rt = Threads::configured(Config::for_cores(1));
    let (entered, running) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    spawn_maintenance(Box::new(move || {
        entered.send(()).unwrap();
        held.recv().unwrap();
    }))
    .unwrap();
    running.recv_timeout(Duration::from_secs(5)).unwrap();
    let (finished, foreground) = std::sync::mpsc::channel();
    block_on(rt.spawn_blocking(
        BlockingClass::Cpu,
        0,
        Box::new(move || finished.send(()).unwrap()),
    ))
    .unwrap();
    let result = foreground.recv_timeout(Duration::from_secs(5));
    release.send(()).unwrap();
    result.expect("foreground work must finish while maintenance is still blocked");
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
fn cancelling_a_credit_waiter_never_releases_another_owners_bytes() {
    let budget = Budget::new(10);
    let held = block_on(budget.acquire(6));
    let shared = budget.clone();
    let mut cancelled = Box::pin(shared.acquire(5));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(cancelled.as_mut().poll(&mut cx).is_pending());
    drop(cancelled);
    let remaining = block_on(shared.acquire(4));
    let mut next = Box::pin(budget.acquire(1));
    assert!(next.as_mut().poll(&mut cx).is_pending());
    drop(remaining);
    let next = block_on(next);
    let mut all = Box::pin(budget.acquire(10));
    assert!(all.as_mut().poll(&mut cx).is_pending());
    drop(next);
    assert!(all.as_mut().poll(&mut cx).is_pending());
    drop(held);
    drop(block_on(all));
}

#[test]
fn oversized_retained_data_runs_alone_without_being_refused() {
    let budget = Budget::new(10);
    let held = block_on(budget.acquire(1));
    let mut oversized = Box::pin(budget.acquire(usize::MAX));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(oversized.as_mut().poll(&mut cx).is_pending());
    drop(held);
    let oversized = block_on(oversized);
    let mut small = Box::pin(budget.acquire(1));
    assert!(small.as_mut().poll(&mut cx).is_pending());
    drop(oversized);
    drop(block_on(small));
}

#[test]
fn accepted_workers_keep_credit_after_their_caller_is_cancelled() {
    let rt = Threads::new(1);
    let budget = Budget::new(10);
    let credit = block_on(budget.acquire(10));
    let (started, running) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let mut work = Box::pin(try_unblock_with(&rt, BlockingClass::Cpu, 10, move || {
        started.send(()).unwrap();
        released.recv().unwrap();
        credit
    }));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(work.as_mut().poll(&mut cx).is_pending());
    running.recv_timeout(Duration::from_secs(3)).unwrap();
    drop(work);
    let mut next = Box::pin(budget.acquire(1));
    assert!(next.as_mut().poll(&mut cx).is_pending());
    // Waiting retained data cannot occupy a storage worker.
    assert_eq!(block_on(unblock(&rt, || 7)), 7);
    release.send(()).unwrap();
    drop(block_on(next));
}

#[test]
fn blocking_pool_runs_work_concurrently() {
    let rt = Threads::configured(Config::for_cores(4));
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let together = Arc::new(std::sync::Barrier::new(2));
    let jobs: Vec<_> = (0..4)
        .map(|_| {
            let (running, peak) = (Arc::clone(&running), Arc::clone(&peak));
            let together = Arc::clone(&together);
            let job = unblock(&rt, move || {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                together.wait();
                running.fetch_sub(1, Ordering::SeqCst);
            });
            spawn(&rt, job)
        })
        .collect();
    block_on(async {
        for job in jobs {
            job.await;
        }
    });
    assert_eq!(peak.load(Ordering::SeqCst), 2);
}

#[test]
fn a_bounded_channel_streams_and_ends() {
    let rt = Threads::new(1);
    let (tx, rx) = channel::bounded(2);
    block_on(rt.spawn_blocking(
        BlockingClass::Storage,
        0,
        Box::new(move || {
            for n in 0..10 {
                tx.send(n).unwrap();
            }
        }),
    ))
    .unwrap();
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
fn a_oneshot_hands_over_its_value_or_ends_with_none_unsent() {
    let (tx, rx) = oneshot::channel();
    thread::spawn(move || tx.send(String::from("done")));
    assert_eq!(block_on(rx).as_deref(), Some("done"));
    let (tx, rx) = oneshot::channel::<u8>();
    thread::spawn(move || drop(tx));
    assert_eq!(block_on(rx), None);
}

#[test]
fn timers_do_not_complete_before_their_deadlines() {
    let rt = Threads::new(1);
    let jobs: Vec<_> = [30, 10, 20, 10]
        .into_iter()
        .map(|ms| {
            let sleep = rt.sleep(Duration::from_millis(ms));
            spawn(&rt, async move {
                let started = Instant::now();
                sleep.await;
                assert!(started.elapsed() >= Duration::from_millis(ms));
            })
        })
        .collect();
    block_on(async {
        for job in jobs {
            job.await;
        }
    });
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

#[test]
fn a_timer_wakes_at_its_deadline_not_before() {
    let rt = Threads::new(1);
    let started = Instant::now();
    block_on(rt.sleep(Duration::from_millis(30)));
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(30), "{took:?}");
    assert!(took < Duration::from_millis(500), "{took:?}");
    // A deadline earlier than the one the reactor waits for still fires
    // on time.
    let mut late = pin!(rt.sleep(Duration::from_secs(30)));
    assert!(
        late.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let started = Instant::now();
    block_on(rt.sleep(Duration::from_millis(10)));
    assert!(started.elapsed() < Duration::from_millis(500));
}

/// Reads a pipe through `rt`'s reactor while another thread writes it.
#[cfg(unix)]
fn read_a_pipe(rt: &dyn Runtime) -> Vec<u8> {
    use std::io::{Read, Write};
    let (reader, mut writer) = std::io::pipe().unwrap();
    let reader = Async::for_reading(&rt.reactor(), reader).unwrap();
    let wrote = thread::spawn(move || {
        for chunk in [&b"one"[..], b"two"] {
            thread::sleep(Duration::from_millis(20));
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
    wrote.join().unwrap();
    got
}

/// A descriptor that turns ready wakes its waiter once or twice, not in a
/// race with the reactor for a newer tick.
#[cfg(unix)]
#[test]
fn readiness_wakes_a_waiter_once_not_in_a_spin() {
    use std::io::Write;
    struct ReadyWake(AtomicUsize, std::sync::mpsc::Sender<()>);
    impl Wake for ReadyWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
            let _ = self.1.send(());
        }
    }
    let rt = Threads::new(1);
    let (reader, mut writer) = std::io::pipe().unwrap();
    let reader = Async::for_reading(&rt.reactor(), reader).unwrap();
    let mut ready = pin!(reader.readable());
    assert!(
        ready
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let (notified, notifications) = std::sync::mpsc::channel();
    let counter = Arc::new(ReadyWake(AtomicUsize::new(0), notified));
    let waker = Waker::from(counter.clone());
    let mut context = Context::from_waker(&waker);
    assert!(ready.as_mut().poll(&mut context).is_pending());
    let (release, held) = std::sync::mpsc::channel();
    let wrote = thread::spawn(move || {
        writer.write_all(b"x").unwrap();
        held.recv().unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while ready.as_mut().poll(&mut context).is_pending() {
        notifications
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
    }
    let woke = counter.0.load(Ordering::Relaxed);
    assert!(woke <= 3, "{woke} wakes");
    release.send(()).unwrap();
    wrote.join().unwrap();
}

#[cfg(unix)]
#[test]
fn a_registered_pipe_reads_when_written_without_a_thread_per_read() {
    assert_eq!(read_a_pipe(&Threads::new(1)), b"onetwo");
}
