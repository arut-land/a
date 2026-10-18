//! One timer thread for every sleep: a deadline heap and the wakers due.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use crate::{lock, wait, wait_timeout};

struct Entry {
    at: Instant,
    seq: u64,
    fired: Arc<AtomicBool>,
    waker: Waker,
}

impl Entry {
    const fn key(&self) -> (Instant, u64) {
        (self.at, self.seq)
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for Entry {}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

#[derive(Default)]
struct Wheel {
    state: Mutex<Deadlines>,
    changed: Condvar,
}

/// Pending wakeups, earliest first; `seq` breaks ties in insertion order.
#[derive(Default)]
struct Deadlines {
    heap: BinaryHeap<Reverse<Entry>>,
    seq: u64,
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
        let mut state = lock(&self.state);
        loop {
            let now = Instant::now();
            while state.heap.peek().is_some_and(|Reverse(e)| e.at <= now) {
                let Reverse(entry) = state.heap.pop().expect("peeked");
                entry.fired.store(true, atomic::Ordering::Release);
                entry.waker.wake();
            }
            state = match state.heap.peek() {
                Some(Reverse(next)) => {
                    let wait = next.at.saturating_duration_since(now);
                    wait_timeout(&self.changed, state, wait).0
                }
                None => wait(&self.changed, state),
            };
        }
    }

    fn insert(&self, at: Instant, fired: Arc<AtomicBool>, waker: Waker) {
        let mut state = lock(&self.state);
        state.seq += 1;
        let seq = state.seq;
        state.heap.push(Reverse(Entry {
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
                .is_some_and(|f| f.load(atomic::Ordering::Acquire))
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
