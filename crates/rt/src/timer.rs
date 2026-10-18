//! One timer thread for every sleep: a deadline heap and the wakers due.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

struct Entry {
    at: Instant,
    seq: u64,
    fired: Arc<AtomicBool>,
    waker: Waker,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}

impl Eq for Entry {}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(other.at, other.seq))
    }
}

#[derive(Default)]
struct Wheel {
    heap: Mutex<(BinaryHeap<Reverse<Entry>>, u64)>,
    changed: Condvar,
}

fn wheel() -> &'static Wheel {
    static WHEEL: OnceLock<&'static Wheel> = OnceLock::new();
    WHEEL.get_or_init(|| {
        let wheel: &'static Wheel = Box::leak(Box::default());
        thread::Builder::new()
            .name("rt-timer".into())
            .spawn(move || wheel.run())
            .expect("spawn timer thread");
        wheel
    })
}

impl Wheel {
    fn run(&self) {
        let mut guard = self.heap.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let now = Instant::now();
            while guard.0.peek().is_some_and(|Reverse(e)| e.at <= now) {
                let Reverse(entry) = guard.0.pop().expect("peeked");
                entry.fired.store(true, Ordering::Release);
                entry.waker.wake();
            }
            guard = match guard.0.peek() {
                Some(Reverse(next)) => {
                    let wait = next.at.saturating_duration_since(now);
                    self.changed
                        .wait_timeout(guard, wait)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
                None => self
                    .changed
                    .wait(guard)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    fn insert(&self, at: Instant, fired: Arc<AtomicBool>, waker: Waker) {
        let mut guard = self.heap.lock().unwrap_or_else(PoisonError::into_inner);
        guard.1 += 1;
        let seq = guard.1;
        guard.0.push(Reverse(Entry {
            at,
            seq,
            fired,
            waker,
        }));
        self.changed.notify_one();
    }
}

pub struct Sleep {
    at: Instant,
    fired: Option<Arc<AtomicBool>>,
}

pub fn sleep(duration: Duration) -> Sleep {
    Sleep {
        at: Instant::now() + duration,
        fired: None,
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.at
            || self
                .fired
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Acquire))
        {
            return Poll::Ready(());
        }
        // Each poll re-registers: the newest waker is the one to wake.
        let fired = Arc::new(AtomicBool::new(false));
        wheel().insert(self.at, Arc::clone(&fired), cx.waker().clone());
        self.fired = Some(fired);
        Poll::Pending
    }
}
