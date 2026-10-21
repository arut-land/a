//! Owned async workers and independently admitted blocking classes. Idle
//! workers retire after one idle timeout; shutdown closes admission before joining.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};

use crate::{BlockingClass, BoxFuture, Reactor, Runtime, Wakeup, lock};
use async_executor::Executor;

type Work = Box<dyn FnOnce() + Send>;

#[derive(Clone, Copy, Debug)]
pub struct PoolLimits {
    pub workers: usize,
    pub queue: usize,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub workers: usize,
    pub storage: PoolLimits,
    pub network: PoolLimits,
    pub cpu: PoolLimits,
}

impl Config {
    pub fn for_cores(cores: usize) -> Self {
        Self {
            workers: cores.clamp(1, 4),
            storage: PoolLimits {
                workers: 2,
                queue: 16,
                bytes: 64 << 20,
            },
            network: PoolLimits {
                workers: 8,
                queue: 16,
                bytes: 32 << 20,
            },
            cpu: PoolLimits {
                workers: 2,
                queue: 8,
                bytes: 32 << 20,
            },
        }
    }
}

pub struct Threads {
    config: Config,
    workers: Mutex<Workers>,
    blocking: [Arc<Blocking>; 4],
}

struct Workers {
    tasks: Option<Arc<Executor<'static>>>,
    threads: Vec<(Arc<Wakeup>, JoinHandle<()>)>,
}

struct Blocking {
    limits: PoolLimits,
    #[cfg(test)]
    start_gate: Mutex<Option<Arc<Gate>>>,
    state: Mutex<Pool>,
    ready: Condvar,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Default)]
struct Pool {
    workers: usize,
    starting: usize,
    idle: usize,
    queue: VecDeque<(usize, Work)>,
    closed: bool,
    bytes: usize,
    next: u64,
    waiters: BTreeMap<u64, Waker>,
}

impl Threads {
    pub fn new(workers: usize) -> Self {
        Self::configured(Config {
            workers: workers.max(1),
            ..Config::for_cores(workers)
        })
    }

    pub fn configured(config: Config) -> Self {
        assert!(config.workers > 0);
        Self {
            config,
            workers: Mutex::new(Workers {
                tasks: Some(Arc::default()),
                threads: Vec::new(),
            }),
            blocking: [
                config.storage,
                config.network,
                config.cpu,
                PoolLimits {
                    workers: 1,
                    queue: 1,
                    bytes: 1,
                },
            ]
            .map(|limits| {
                assert!(limits.workers > 0 && limits.queue > 0 && limits.bytes > 0);
                Arc::new(Blocking {
                    limits,
                    #[cfg(test)]
                    start_gate: Mutex::default(),
                    state: Mutex::default(),
                    ready: Condvar::new(),
                    threads: Mutex::default(),
                })
            }),
        }
    }

    /// Call from the owner after cancelling external I/O. Running closures
    /// cannot be forcibly stopped; they finish before their pool is reclaimed.
    pub fn shutdown(&self) {
        let mut workers = lock(&self.workers);
        let tasks = workers.tasks.take();
        let threads = std::mem::take(&mut workers.threads);
        drop(workers);
        for pool in &self.blocking {
            let mut state = lock(&pool.state);
            state.closed = true;
            pool.ready.notify_all();
            wake(&mut state);
        }
        for (stop, _) in &threads {
            stop.notify();
        }
        for (_, thread) in threads {
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
        drop(tasks);
        for pool in &self.blocking {
            let threads = std::mem::take(&mut *lock(&pool.threads));
            for worker in threads {
                if worker.thread().id() != thread::current().id() {
                    let _ = worker.join();
                }
            }
        }
    }
}

impl Drop for Threads {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Default for Threads {
    fn default() -> Self {
        Self::configured(Config::for_cores(
            thread::available_parallelism().map_or(1, usize::from),
        ))
    }
}

impl Runtime for Threads {
    fn spawn(&self, task: BoxFuture<'static, ()>) {
        let mut workers = lock(&self.workers);
        let Some(tasks) = workers.tasks.clone() else {
            return;
        };
        if workers.threads.is_empty() {
            for n in 0..self.config.workers {
                let tasks = Arc::clone(&tasks);
                let stop = Arc::new(Wakeup::new());
                let stopped = Arc::clone(&stop);
                let handle = thread::Builder::new()
                    .name(format!("rt-worker-{n}"))
                    .spawn(move || crate::block_on(tasks.run(stopped.wait())))
                    .expect("spawn async worker");
                workers.threads.push((stop, handle));
            }
        }
        tasks.spawn(task).detach();
    }

    fn spawn_blocking(
        &self,
        class: BlockingClass,
        bytes: usize,
        work: Work,
    ) -> BoxFuture<'static, io::Result<()>> {
        Box::pin(Admission {
            pool: Arc::clone(&self.blocking[class as usize]),
            bytes,
            work: Some(work),
            waiter: None,
        })
    }

    fn spawn_flush(&self, work: Work) -> BoxFuture<'static, io::Result<()>> {
        Box::pin(Admission {
            pool: Arc::clone(&self.blocking[3]),
            bytes: 0,
            work: Some(work),
            waiter: None,
        })
    }

    fn reactor(&self) -> Reactor {
        Reactor::AsyncIo
    }
}

struct Admission {
    pool: Arc<Blocking>,
    bytes: usize,
    work: Option<Work>,
    waiter: Option<u64>,
}

impl Future for Admission {
    type Output = io::Result<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut state = lock(&this.pool.state);
        if state.closed || this.bytes > this.pool.limits.bytes {
            return Poll::Ready(Err(io::Error::other(
                "blocking admission closed or reservation exceeds budget",
            )));
        }
        if state.queue.len() == this.pool.limits.queue
            || state.bytes > this.pool.limits.bytes - this.bytes
        {
            let key = *this.waiter.get_or_insert_with(|| {
                state.next += 1;
                state.next
            });
            state.waiters.insert(key, cx.waker().clone());
            return Poll::Pending;
        }
        if let Some(key) = this.waiter.take() {
            state.waiters.remove(&key);
        }
        state.bytes += this.bytes;
        state
            .queue
            .push_back((this.bytes, this.work.take().expect("admitted once")));
        if state.queue.len() > state.idle + state.starting
            && state.workers + state.starting < this.pool.limits.workers
        {
            state.starting += 1;
            let pool = Arc::clone(&this.pool);
            // Count the thread before releasing the queue lock: bursts must
            // not mistake workers still starting for missing capacity.
            match thread::Builder::new()
                .name("rt-blocking".into())
                .spawn(move || pool.serve())
            {
                Ok(worker) => {
                    let mut threads = lock(&this.pool.threads);
                    threads.retain(|worker| !worker.is_finished());
                    threads.push(worker);
                }
                Err(error) => {
                    state.starting -= 1;
                    let (bytes, _) = state.queue.pop_back().expect("just submitted");
                    state.bytes -= bytes;
                    wake(&mut state);
                    return Poll::Ready(Err(error));
                }
            }
        }
        this.pool.ready.notify_one();
        Poll::Ready(Ok(()))
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if let Some(key) = self.waiter {
            lock(&self.pool.state).waiters.remove(&key);
        }
    }
}

fn wake(state: &mut Pool) {
    for (_, waker) in std::mem::take(&mut state.waiters) {
        waker.wake();
    }
}

impl Blocking {
    fn serve(&self) {
        #[cfg(test)]
        if let Some(gate) = lock(&self.start_gate).clone() {
            gate.wait();
        }
        let mut state = lock(&self.state);
        state.starting -= 1;
        state.workers += 1;
        loop {
            if let Some((bytes, work)) = state.queue.pop_front() {
                wake(&mut state);
                drop(state);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                state = lock(&self.state);
                state.bytes -= bytes;
                wake(&mut state);
            } else if state.closed {
                state.workers -= 1;
                return;
            } else {
                state.idle += 1;
                let (next, timeout) = self
                    .ready
                    .wait_timeout(state, std::time::Duration::from_secs(10))
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state = next;
                state.idle -= 1;
                if timeout.timed_out() && state.queue.is_empty() {
                    state.workers -= 1;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct Gate(Mutex<bool>, Condvar);
#[cfg(test)]
impl Gate {
    fn wait(&self) {
        let mut open = lock(&self.0);
        while !*open {
            open = crate::wait(&self.1, open);
        }
    }
    fn open(&self) {
        *lock(&self.0) = true;
        self.1.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{block_on, oneshot, spawn};
    use std::time::Duration;

    #[test]
    fn submission_bursts_count_starting_workers() {
        let rt = Threads::new(1);
        let pool = &rt.blocking[0];
        let gate = Arc::new(Gate::default());
        *lock(&pool.start_gate) = Some(Arc::clone(&gate));
        for _ in 0..16 {
            block_on(rt.spawn_blocking(BlockingClass::Storage, 1, Box::new(|| {}))).unwrap();
        }
        let state = lock(&pool.state);
        let observed = (state.workers, state.starting, state.queue.len());
        drop(state);
        gate.open();
        rt.shutdown();
        assert_eq!(observed, (0, 2, 16));
    }

    #[test]
    fn saturated_network_work_cannot_starve_a_flush() {
        let mut config = Config::for_cores(2);
        config.network = PoolLimits {
            workers: 1,
            queue: 1,
            bytes: 2,
        };
        let rt = Threads::configured(config);
        let gate = Arc::new(Gate::default());
        let held = Arc::clone(&gate);
        let (entered, running) = std::sync::mpsc::channel();
        block_on(rt.spawn_blocking(
            BlockingClass::Network,
            1,
            Box::new(move || {
                entered.send(()).unwrap();
                held.wait();
            }),
        ))
        .unwrap();
        running.recv_timeout(Duration::from_secs(2)).unwrap();
        block_on(rt.spawn_blocking(BlockingClass::Network, 1, Box::new(|| {}))).unwrap();
        let mut pending = rt.spawn_blocking(BlockingClass::Network, 1, Box::new(|| {}));
        assert!(
            pending
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let (flushed, done) = std::sync::mpsc::channel();
        let epoch = crate::Flusher::default().request(&rt, move || {
            flushed.send(()).unwrap();
            Ok(())
        });
        let result = done.recv_timeout(Duration::from_secs(2));
        gate.open();
        result.unwrap();
        block_on(epoch).unwrap();
        drop(pending);
        rt.shutdown();
    }

    #[test]
    fn runtime_shutdown_reclaims_workers() {
        let rt = Threads::new(2);
        block_on(spawn(&rt, async {}));
        for class in [
            BlockingClass::Storage,
            BlockingClass::Network,
            BlockingClass::Cpu,
        ] {
            block_on(crate::unblock_with(&rt, class, 1, || ()));
        }
        let (tx, rx) = oneshot::channel();
        rt.spawn(Box::pin(async move {
            let _held = tx;
            std::future::pending::<()>().await;
        }));
        rt.shutdown();
        assert_eq!(block_on(rx), None::<()>);
        assert!(lock(&rt.workers).threads.is_empty());
        for pool in &rt.blocking {
            let state = lock(&pool.state);
            assert_eq!((state.workers, state.starting, state.bytes), (0, 0, 0));
            assert!(state.closed && state.queue.is_empty());
        }
        assert!(
            block_on(rt.spawn_blocking(BlockingClass::Storage, 0, Box::new(|| panic!("closed"))))
                .is_err()
        );
    }

    #[test]
    fn reservations_and_cancelled_waiters_release_capacity() {
        let mut config = Config::for_cores(1);
        config.cpu = PoolLimits {
            workers: 1,
            queue: 4,
            bytes: 10,
        };
        let rt = Threads::configured(config);
        let gate = Arc::new(Gate::default());
        let held = Arc::clone(&gate);
        block_on(rt.spawn_blocking(BlockingClass::Cpu, 10, Box::new(move || held.wait()))).unwrap();
        let mut pending =
            rt.spawn_blocking(BlockingClass::Cpu, 1, Box::new(|| panic!("cancelled")));
        assert!(
            pending
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(pending);
        assert!(lock(&rt.blocking[2].state).waiters.is_empty());
        assert!(block_on(rt.spawn_blocking(BlockingClass::Cpu, 11, Box::new(|| {}))).is_err());
        gate.open();
        block_on(crate::unblock_with(&rt, BlockingClass::Cpu, 10, || ()));
        rt.shutdown();
    }

    #[test]
    fn memory_budgets_do_not_scale_with_core_count() {
        for cores in [2, 4, 8, 32] {
            let config = Config::for_cores(cores);
            assert_eq!(config.workers, cores.min(4));
            assert_eq!(
                (
                    config.storage.workers,
                    config.network.workers,
                    config.cpu.workers
                ),
                (2, 8, 2)
            );
            assert_eq!(
                config.storage.bytes + config.network.bytes + config.cpu.bytes,
                128 << 20
            );
        }
    }
}
