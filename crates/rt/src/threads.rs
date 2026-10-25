//! Owned async workers and independently admitted blocking classes. Idle
//! workers retire after one idle timeout; shutdown closes admission and bounds reclamation.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
    /// Blocking jobs run at the platform's foreground class (user-initiated
    /// quality of service on macOS). The daemon asks for it; a client
    /// inherits its caller's.
    pub foreground: bool,
}

impl Config {
    pub fn for_cores(cores: usize) -> Self {
        Self {
            workers: cores.clamp(1, 4),
            storage: PoolLimits {
                workers: (cores / 2).clamp(1, 4),
                queue: 16,
                bytes: 64 << 20,
            },
            network: PoolLimits {
                workers: 8,
                queue: 16,
                bytes: 32 << 20,
            },
            cpu: PoolLimits {
                workers: cores.saturating_sub(1).max(1),
                queue: 8,
                bytes: 64 << 20,
            },
            foreground: false,
        }
    }

    /// The daemon's runtime: native CPU limits (Windows CPU sets and job
    /// caps) and foreground blocking jobs. Clients keep the plain default,
    /// which costs them neither the discovery nor a class change per job.
    pub fn daemon() -> Self {
        Self {
            foreground: true,
            ..Self::for_cores(sys::resources::effective_parallelism())
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::for_cores(thread::available_parallelism().map_or(1, usize::from))
    }
}

pub struct Threads {
    config: Config,
    lifecycle: Arc<Lifecycle>,
    workers: Mutex<Workers>,
    blocking: [Arc<Blocking>; 4],
}

struct Workers {
    tasks: Option<Arc<Executor<'static>>>,
    threads: Vec<(Arc<Wakeup>, JoinHandle<()>)>,
}

// The pinned scheduler owns its State, so concurrent destruction is sound.
// Holding the executor during scheduling prevents enqueueing after its final
// drain, which would leak a State/queued-task cycle. A weak reference avoids
// retaining the executor through its tasks and makes late wakes no-ops.
struct TaskWake {
    executor: Weak<Executor<'static>>,
    task: Waker,
}

impl Wake for TaskWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(_executor) = self.executor.upgrade() {
            self.task.wake_by_ref();
        }
    }
}

fn guarded_task(
    executor: &Arc<Executor<'static>>,
    mut task: BoxFuture<'static, ()>,
) -> impl Future<Output = ()> + Send + 'static {
    let executor = Arc::downgrade(executor);
    // async-executor polls each task with its fixed async-task scheduler waker.
    // Cache the wrapper itself: no allocation or refcount traffic on later polls.
    let mut waker = None;
    std::future::poll_fn(move |cx| {
        let waker = waker.get_or_insert_with(|| {
            Waker::from(Arc::new(TaskWake {
                executor: executor.clone(),
                task: cx.waker().clone(),
            }))
        });
        task.as_mut().poll(&mut Context::from_waker(waker))
    })
}

#[derive(Default)]
struct Lifecycle {
    running: Mutex<usize>,
    ended: Condvar,
}

struct Worker(Arc<Lifecycle>);

impl Lifecycle {
    fn worker(self: &Arc<Self>) -> Worker {
        *lock(&self.running) += 1;
        Worker(Arc::clone(self))
    }

    fn wait_until(&self, deadline: Instant) {
        let mut running = lock(&self.running);
        while *running != 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (next, _) = self
                .ended
                .wait_timeout(running, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            running = next;
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        *lock(&self.0.running) -= 1;
        self.0.ended.notify_all();
    }
}

struct Blocking {
    limits: PoolLimits,
    foreground: bool,
    lifecycle: Arc<Lifecycle>,
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
            ..Config::default()
        })
    }

    pub fn configured(config: Config) -> Self {
        assert!(config.workers > 0);
        let lifecycle = Arc::new(Lifecycle::default());
        Self {
            config,
            lifecycle: Arc::clone(&lifecycle),
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
                    foreground: config.foreground,
                    lifecycle: Arc::clone(&lifecycle),
                    #[cfg(test)]
                    start_gate: Mutex::default(),
                    state: Mutex::default(),
                    ready: Condvar::new(),
                    threads: Mutex::default(),
                })
            }),
        }
    }

    /// Best-effort reclamation; a native I/O stall cannot hold the owner
    /// indefinitely. Call `shutdown_until` to share an existing deadline.
    pub fn shutdown(&self) {
        self.shutdown_until(Instant::now() + Duration::from_millis(250));
    }

    /// Close admission and signal every worker. Only finished threads may
    /// be joined: at the deadline, running native closures must be detached.
    pub fn shutdown_until(&self, deadline: Instant) {
        let mut workers = lock(&self.workers);
        let tasks = workers.tasks.take();
        let threads = std::mem::take(&mut workers.threads);
        drop(workers);
        if tasks.is_none() && threads.is_empty() {
            return;
        }
        for pool in &self.blocking {
            let mut state = lock(&pool.state);
            state.closed = true;
            pool.ready.notify_all();
            wake(&mut state);
        }
        let blocking: Vec<_> = self
            .blocking
            .iter()
            .flat_map(|pool| std::mem::take(&mut *lock(&pool.threads)))
            .collect();
        let current = thread::current().id();
        let on_worker = threads
            .iter()
            .any(|(_, worker)| worker.thread().id() == current)
            || blocking
                .iter()
                .any(|worker| worker.thread().id() == current);
        // Executors cancel their futures on their workers, where a blocking
        // destructor cannot consume the owner's shutdown deadline.
        drop(tasks);
        for (stop, _) in &threads {
            stop.notify();
        }
        if !on_worker {
            self.lifecycle.wait_until(deadline);
        }
        for worker in threads
            .into_iter()
            .map(|(_, worker)| worker)
            .chain(blocking)
        {
            if worker.is_finished() {
                let _ = worker.join();
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
        Self::configured(Config::default())
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
                let worker = self.lifecycle.worker();
                let handle = thread::Builder::new()
                    .name(format!("rt-worker-{n}"))
                    .spawn(move || {
                        let run = tasks.run(stopped.wait());
                        // TEMPORARY (B1 measurement): the reference layout parks as main does.
                        if std::env::var_os("A_RT_PARK").is_some_and(|v| v == "thread") {
                            crate::block_on(run);
                        } else {
                            // A parked worker waits in the reactor itself, so
                            // an event wakes the thread that runs its task;
                            // async-io's thread waits only while every
                            // worker is busy.
                            async_io::block_on(crate::counted(run));
                        }
                        drop(tasks);
                        drop(worker);
                    })
                    .expect("spawn async worker");
                workers.threads.push((stop, handle));
            }
        }
        tasks.spawn(guarded_task(&tasks, task)).detach();
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
            let worker = this.pool.lifecycle.worker();
            match thread::Builder::new()
                .name("rt-blocking".into())
                .spawn(move || {
                    pool.serve();
                    drop(pool);
                    drop(worker);
                }) {
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
                let priority = self
                    .foreground
                    .then(sys::resources::Priority::user_initiated);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                drop(priority);
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
                    .wait_timeout(state, Duration::from_secs(10))
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

    #[test]
    fn task_wakes_can_race_executor_drop() {
        for _ in 0..100 {
            let executor = Arc::new(Executor::new());
            let (sent, received) = std::sync::mpsc::channel();
            let task = Box::pin(std::future::poll_fn(move |cx| {
                sent.send(cx.waker().clone()).unwrap();
                Poll::<()>::Pending
            }));
            executor.spawn(guarded_task(&executor, task)).detach();
            assert!(executor.try_tick());
            let wake = received.recv().unwrap();
            let late = wake.clone();
            let gone = Arc::downgrade(&executor);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let start = Arc::clone(&barrier);
            let worker = thread::spawn(move || {
                start.wait();
                wake.wake();
            });
            barrier.wait();
            drop(executor);
            worker.join().unwrap();
            assert!(
                gone.upgrade().is_none(),
                "wakers cannot retain the executor"
            );
            late.wake_by_ref();
            late.wake();
        }
    }

    #[test]
    fn task_waker_is_reused_when_the_scheduler_waker_is_unchanged() {
        let executor = Arc::new(Executor::new());
        let (sent, received) = std::sync::mpsc::channel();
        let task = Box::pin(std::future::poll_fn(move |cx| {
            sent.send(cx.waker().clone()).unwrap();
            Poll::<()>::Pending
        }));
        executor.spawn(guarded_task(&executor, task)).detach();
        assert!(executor.try_tick());
        let first = received.recv().unwrap();
        for _ in 0..3 {
            first.wake_by_ref();
            assert!(executor.try_tick());
            let next = received.recv().unwrap();
            // Compare allocations: will_wake permits false negatives.
            assert_eq!(first.data(), next.data());
        }
    }

    #[test]
    fn submission_bursts_count_starting_workers() {
        let rt = Threads::configured(Config::for_cores(4));
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
    fn shutdown_deadline_detaches_stalled_work_and_keeps_admission_closed() {
        let rt = Arc::new(Threads::new(1));
        let (release, released) = std::sync::mpsc::channel();
        let (entered, running) = std::sync::mpsc::channel();
        let (finished, ended) = std::sync::mpsc::channel();
        block_on(rt.spawn_blocking(
            BlockingClass::Storage,
            1,
            Box::new(move || {
                entered.send(()).unwrap();
                released.recv().unwrap();
                finished.send(()).unwrap();
            }),
        ))
        .unwrap();
        running.recv_timeout(Duration::from_secs(2)).unwrap();
        let owner = Arc::clone(&rt);
        let (stopped, done) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            owner.shutdown_until(Instant::now());
            owner.shutdown();
            stopped.send(()).unwrap();
        });
        let result = done.recv_timeout(Duration::from_millis(500));
        release.send(()).unwrap();
        ended.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        result.expect("an expired deadline cannot join a stalled worker");
        assert!(block_on(rt.spawn_blocking(BlockingClass::Storage, 0, Box::new(|| {}))).is_err());
    }

    #[test]
    fn the_last_handle_can_drop_on_a_worker_without_joining_its_own_cancellation() {
        struct Release(std::sync::mpsc::Sender<()>);
        impl Drop for Release {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let rt = Arc::new(Threads::new(1));
        let (release, released) = std::sync::mpsc::channel();
        let guard = Release(release);
        rt.spawn(Box::pin(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        }));
        let runtime = Arc::clone(&rt);
        let (go, started) = oneshot::channel();
        let (done, dropped) = std::sync::mpsc::channel();
        let (finished, ended) = std::sync::mpsc::channel();
        rt.spawn(Box::pin(async move {
            started.await.unwrap();
            runtime
                .spawn_blocking(
                    BlockingClass::Storage,
                    0,
                    Box::new(move || {
                        released.recv_timeout(Duration::from_secs(5)).unwrap();
                        finished.send(()).unwrap();
                    }),
                )
                .await
                .unwrap();
            drop(runtime);
            done.send(()).unwrap();
        }));
        drop(rt);
        go.send(());
        dropped.recv_timeout(Duration::from_secs(2)).unwrap();
        ended.recv_timeout(Duration::from_secs(2)).unwrap();
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
    fn cpu_capacity_follows_demand_within_the_fixed_byte_budget() {
        for (cores, bytes) in [
            (2, 1 << 20),
            (4, 1 << 20),
            (8, 1 << 20),
            (32, 1 << 20),
            (32, crate::STORAGE_BYTES),
        ] {
            let config = Config::for_cores(cores);
            let expected = config.cpu.workers.min(config.cpu.bytes / bytes);
            let runtime = Threads::configured(config);
            let pool = &runtime.blocking[BlockingClass::Cpu as usize];
            assert_eq!(lock(&pool.state).workers, 0, "no workers before demand");
            let gate = Arc::new(Gate::default());
            let (entered, running) = std::sync::mpsc::channel();
            for _ in 0..expected {
                let held = Arc::clone(&gate);
                let entered = entered.clone();
                block_on(runtime.spawn_blocking(
                    BlockingClass::Cpu,
                    bytes,
                    Box::new(move || {
                        entered.send(()).unwrap();
                        held.wait();
                    }),
                ))
                .unwrap();
                running.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            let state = lock(&pool.state);
            let observed = (state.workers, state.starting, state.bytes);
            drop(state);
            gate.open();
            runtime.shutdown();
            assert_eq!(observed, (expected, 0, expected * bytes));
        }
    }

    #[test]
    fn only_the_daemon_runs_blocking_jobs_at_the_foreground_class() {
        assert!(!Config::default().foreground);
        assert!(!Config::for_cores(4).foreground);
        assert!(Config::daemon().foreground);
    }

    #[test]
    fn memory_budgets_do_not_scale_with_core_count() {
        for cores in [0, 1, 2, 4, 8, 32] {
            let config = Config::for_cores(cores);
            assert_eq!(config.workers, cores.clamp(1, 4));
            assert_eq!(
                (
                    config.storage.workers,
                    config.network.workers,
                    config.cpu.workers
                ),
                ((cores / 2).clamp(1, 4), 8, cores.saturating_sub(1).max(1))
            );
            assert_eq!(
                config.storage.bytes + config.network.bytes + config.cpu.bytes,
                160 << 20
            );
        }
    }
}
