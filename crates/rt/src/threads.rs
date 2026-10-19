//! The built-in runtime: a few async workers, a pool of blocking threads
//! that grows on demand and shrinks when idle, and the reactor's timers.
//! Threads start on first use, so an idle process pays for none.

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use async_task::Runnable;

use crate::{BoxFuture, Runtime, lock, wait, wait_timeout};

/// How long a blocking thread waits for more work before it exits.
const IDLE: Duration = Duration::from_secs(10);

pub struct Threads {
    workers: usize,
    started: OnceLock<()>,
    tasks: Arc<Tasks>,
    blocking: Arc<Blocking>,
}

/// Runnable tasks for the async workers.
#[derive(Default)]
struct Tasks {
    queue: Mutex<VecDeque<Runnable>>,
    ready: Condvar,
}

impl Tasks {
    fn push(&self, task: Runnable) {
        lock(&self.queue).push_back(task);
        self.ready.notify_one();
    }

    fn pop(&self) -> Runnable {
        let mut queue = lock(&self.queue);
        loop {
            if let Some(task) = queue.pop_front() {
                return task;
            }
            queue = wait(&self.ready, queue);
        }
    }
}

type Work = Box<dyn FnOnce() + Send>;

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
                thread::Builder::new()
                    .name(format!("rt-worker-{n}"))
                    .spawn(move || {
                        // A task that panics is closed; the worker lives on.
                        loop {
                            let task = tasks.pop();
                            let _ = panic::catch_unwind(AssertUnwindSafe(|| task.run()));
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

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(crate::Timer::after(duration))
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
