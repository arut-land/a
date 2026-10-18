//! The built-in runtime: a few async workers, a pool of blocking threads
//! that grows on demand and shrinks when idle, and the shared timer thread.
//! Threads start on first use, so an idle process pays for none.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::Duration;

use async_task::Runnable;

use crate::{BoxFuture, Runtime};

/// How long a blocking thread waits for more work before it exits.
const IDLE: Duration = Duration::from_secs(10);

pub struct Threads {
    workers: usize,
    started: OnceLock<()>,
    tasks: Arc<Queue<Runnable>>,
    blocking: Arc<Blocking>,
}

struct Queue<T> {
    items: Mutex<VecDeque<T>>,
    ready: Condvar,
}

impl<T> Queue<T> {
    fn new() -> Self {
        Self {
            items: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
        }
    }

    fn push(&self, item: T) {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(item);
        self.ready.notify_one();
    }

    fn pop(&self) -> T {
        let mut items = self.items.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(item) = items.pop_front() {
                return item;
            }
            items = self
                .ready
                .wait(items)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

type Work = Box<dyn FnOnce() + Send>;

/// Queued work and the number of threads waiting for it.
struct Blocking {
    state: Mutex<(VecDeque<Work>, usize)>,
    ready: Condvar,
}

impl Threads {
    /// A runtime with `workers` async worker threads.
    pub fn new(workers: usize) -> Self {
        Self {
            workers: workers.max(1),
            started: OnceLock::new(),
            tasks: Arc::new(Queue::new()),
            blocking: Arc::new(Blocking {
                state: Mutex::new((VecDeque::new(), 0)),
                ready: Condvar::new(),
            }),
        }
    }

    fn start(&self) {
        self.started.get_or_init(|| {
            for n in 0..self.workers {
                let tasks = Arc::clone(&self.tasks);
                thread::Builder::new()
                    .name(format!("rt-worker-{n}"))
                    .spawn(move || {
                        loop {
                            tasks.pop().run();
                        }
                    })
                    .expect("spawn worker thread");
            }
        });
    }
}

impl Default for Threads {
    fn default() -> Self {
        let cores = thread::available_parallelism().map_or(1, usize::from);
        Self::new(cores.min(4))
    }
}

impl Runtime for Threads {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        self.start();
        let tasks = Arc::clone(&self.tasks);
        let (runnable, handle) = async_task::spawn(task, move |runnable| tasks.push(runnable));
        handle.detach();
        runnable.schedule();
    }

    fn spawn_blocking(&self, work: Work) {
        let blocking = &self.blocking;
        let mut state = blocking
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.0.push_back(work);
        // An idle thread is waiting whenever the queue was empty and some
        // thread is parked; otherwise grow by one.
        let (queued, idle) = (state.0.len(), state.1);
        if idle >= queued {
            blocking.ready.notify_one();
            return;
        }
        drop(state);
        let pool = Arc::clone(blocking);
        thread::Builder::new()
            .name("rt-blocking".into())
            .spawn(move || pool.serve())
            .expect("spawn blocking thread");
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(crate::timer::sleep(duration))
    }
}

impl Blocking {
    fn serve(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(work) = state.0.pop_front() {
                drop(state);
                work();
                state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            state.1 += 1;
            let (next, timeout) = self
                .ready
                .wait_timeout(state, IDLE)
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            state.1 -= 1;
            if timeout.timed_out() && state.0.is_empty() {
                return;
            }
        }
    }
}
