//! The built-in runtime: async-executor's work-stealing workers, a pool of
//! blocking threads that grows on demand and shrinks when idle, and
//! async-io's reactor and timers. Threads start on first use, so an idle
//! process pays for none, and none wakes until there is work.

use std::collections::VecDeque;
use std::future;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use async_executor::Executor;

use crate::{BoxFuture, Reactor, Runtime, lock, wait_timeout};

/// How long a blocking thread waits for more work before it exits.
const IDLE: Duration = Duration::from_secs(10);

pub struct Threads {
    workers: usize,
    started: OnceLock<()>,
    tasks: Arc<Executor<'static>>,
    blocking: Arc<Blocking>,
}

type Work = Box<dyn FnOnce() + Send>;

/// Ours rather than the `blocking` crate's, whose last thread polls every
/// 500 ms while it waits for work.
#[derive(Default)]
struct Blocking {
    state: Mutex<Pool>,
    ready: Condvar,
}

/// Queued work and the number of threads waiting for it.
#[derive(Default)]
struct Pool {
    queue: VecDeque<Work>,
    idle: usize,
}

impl Threads {
    /// A runtime with `workers` async worker threads.
    pub fn new(workers: usize) -> Self {
        Self {
            workers: workers.max(1),
            started: OnceLock::new(),
            tasks: Arc::default(),
            blocking: Arc::default(),
        }
    }

    fn start(&self) {
        self.started.get_or_init(|| {
            for n in 0..self.workers {
                let tasks = Arc::clone(&self.tasks);
                // A task that panics is closed; the worker lives on. Parked
                // in `block_on` rather than async-io's, whose reactor thread
                // polls while any thread is.
                thread::Builder::new()
                    .name(format!("rt-worker-{n}"))
                    .spawn(move || crate::block_on(tasks.run(future::pending::<()>())))
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
        self.tasks.spawn(task).detach();
    }

    fn spawn_blocking(&self, work: Work) {
        let mut pool = lock(&self.blocking.state);
        pool.queue.push_back(work);
        // Enough threads are waiting to take everything queued; otherwise
        // grow by one.
        if pool.idle >= pool.queue.len() {
            self.blocking.ready.notify_one();
            return;
        }
        drop(pool);
        let blocking = Arc::clone(&self.blocking);
        thread::Builder::new()
            .name("rt-blocking".into())
            .spawn(move || blocking.serve())
            .expect("spawn blocking thread");
    }

    fn reactor(&self) -> Reactor {
        Reactor::AsyncIo
    }
}

impl Blocking {
    fn serve(&self) {
        let mut pool = lock(&self.state);
        loop {
            if let Some(work) = pool.queue.pop_front() {
                drop(pool);
                work();
                pool = lock(&self.state);
                continue;
            }
            pool.idle += 1;
            let timed_out;
            (pool, timed_out) = wait_timeout(&self.ready, pool, IDLE);
            pool.idle -= 1;
            if timed_out && pool.queue.is_empty() {
                return;
            }
        }
    }
}
