//! A cache flush covers barriers from many files. Requests arriving during
//! one syscall share the next epoch, which starts after all their writes.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::{Runtime, lock, oneshot};

type Work = Box<dyn FnOnce() -> io::Result<()> + Send>;
type Prepare = Box<dyn FnOnce() -> Work + Send>;

#[derive(Default)]
struct State {
    running: bool,
    work: Option<Prepare>,
    pending: Vec<oneshot::Sender<io::Result<()>>>,
}

/// One shared flusher, with no timer and at most one blocking job active.
#[derive(Default)]
pub struct Flusher(Arc<Mutex<State>>);

/// A promise that a flush started after the requesting write has completed.
pub struct FlushEpoch(oneshot::Receiver<io::Result<()>>);

impl FlushEpoch {
    pub fn ready(result: io::Result<()>) -> Self {
        let (send, receive) = oneshot::channel();
        send.send(result);
        Self(receive)
    }
}

impl Future for FlushEpoch {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|result| result.unwrap_or_else(|| Err(io::Error::other("cache flusher stopped"))))
    }
}

/// Registration happens after the write barrier on the appending thread.
/// Returning the epoch first lets its actor continue while the leader flushes.
pub struct FlushRequest {
    pub epoch: FlushEpoch,
    pub leader: Option<FlushLeader>,
}

pub struct FlushLeader {
    shared: Arc<Mutex<State>>,
    first: Option<(Work, Vec<oneshot::Sender<io::Result<()>>>)>,
}

impl FlushLeader {
    fn seal(mut self) -> Self {
        if self.first.is_none() {
            let (prepare, pending) = {
                let mut state = lock(&self.shared);
                (
                    state.work.take().expect("elected flush"),
                    std::mem::take(&mut state.pending),
                )
            };
            self.first = Some((prepare(), pending));
        }
        self
    }

    pub fn run(self) {
        let mut leader = self.seal();
        let (work, pending) = leader.first.take().expect("sealed flush");
        complete(work, pending);
        run(&leader.shared);
    }
}

impl FlushRequest {
    pub fn ready(result: io::Result<()>) -> Self {
        Self {
            epoch: FlushEpoch::ready(result),
            leader: None,
        }
    }

    pub fn dispatch(self, rt: &dyn Runtime) -> FlushEpoch {
        if let Some(leader) = self.leader {
            rt.spawn_blocking(Box::new(move || leader.run()));
        }
        self.epoch
    }
}

/// Hands ordered completion back before reusing the blocking thread for its flush.
pub fn unblock_flush<T: Send + 'static>(
    rt: &dyn Runtime,
    work: impl FnOnce() -> (T, Option<FlushLeader>) + Send + 'static,
) -> impl Future<Output = T> + Send + 'static {
    let (send, receive) = oneshot::channel();
    rt.spawn_blocking(Box::new(move || {
        let (ordered, leader) = work();
        let leader = leader.map(FlushLeader::seal);
        send.send(ordered);
        if let Some(leader) = leader {
            leader.run();
        }
    }));
    async move { receive.await.expect("ordered write panicked") }
}

impl Flusher {
    /// `work` must flush the same cache as every other request to this
    /// flusher. Only the first operation in each epoch needs to run.
    pub fn request(
        &self,
        rt: &dyn Runtime,
        work: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> FlushEpoch {
        self.enroll(work).dispatch(rt)
    }

    pub fn enroll(&self, work: impl FnOnce() -> io::Result<()> + Send + 'static) -> FlushRequest {
        self.enroll_prepared(move || Box::new(work))
    }

    /// Captures the epoch's prefix before ordered completion is returned;
    /// the returned operation can block without delaying that completion.
    pub fn enroll_prepared(
        &self,
        prepare: impl FnOnce() -> Box<dyn FnOnce() -> io::Result<()> + Send> + Send + 'static,
    ) -> FlushRequest {
        let (send, receive) = oneshot::channel();
        let mut state = lock(&self.0);
        state.pending.push(send);
        if state.work.is_none() {
            state.work = Some(Box::new(prepare));
        }
        let leader = if state.running {
            None
        } else {
            state.running = true;
            Some(FlushLeader {
                shared: Arc::clone(&self.0),
                first: None,
            })
        };
        FlushRequest {
            epoch: FlushEpoch(receive),
            leader,
        }
    }
}

fn run(shared: &Mutex<State>) {
    loop {
        let (work, pending) = {
            let mut state = lock(shared);
            let Some(work) = state.work.take() else {
                state.running = false;
                return;
            };
            (work, std::mem::take(&mut state.pending))
        };
        complete(work(), pending);
    }
}

fn complete(work: Work, pending: Vec<oneshot::Sender<io::Result<()>>>) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err(io::Error::other("cache flush panicked")));
    for send in pending {
        send.send(
            result
                .as_ref()
                .copied()
                .map_err(|error| io::Error::new(error.kind(), error.to_string())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoxFuture, Reactor};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Manual(Mutex<Option<Box<dyn FnOnce() + Send>>>);

    impl Runtime for Manual {
        fn spawn(&self, _: BoxFuture<'static, ()>) {
            unreachable!()
        }
        fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
            assert!(
                lock(&self.0).replace(work).is_none(),
                "only one flush worker"
            );
        }
        fn reactor(&self) -> Reactor {
            unreachable!()
        }
    }

    #[test]
    fn ordered_completion_precedes_the_flush_on_the_same_blocking_thread() {
        let rt = Manual::default();
        let flusher = Flusher::default();
        let (started, running) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let ordered = unblock_flush(&rt, move || {
            let appender = std::thread::current().id();
            let request = flusher.enroll(move || {
                assert_eq!(std::thread::current().id(), appender);
                started.send(()).unwrap();
                released.recv().unwrap();
                Ok(())
            });
            ((7, request.epoch), request.leader)
        });
        std::thread::scope(|scope| {
            let worker = lock(&rt.0).take().unwrap();
            let worker = scope.spawn(worker);
            running.recv().unwrap();
            let mut ordered = std::pin::pin!(ordered);
            let mut cx = Context::from_waker(std::task::Waker::noop());
            let Poll::Ready((value, epoch)) = ordered.as_mut().poll(&mut cx) else {
                panic!("ordering cannot wait for a blocked full flush");
            };
            assert_eq!(value, 7);
            assert!(lock(&rt.0).is_none(), "no intermediate blocking job");
            release.send(()).unwrap();
            crate::block_on(epoch).unwrap();
            worker.join().unwrap();
        });
    }

    #[test]
    fn concurrent_sessions_share_one_flush() {
        let rt = &Manual::default();
        let flusher = &Flusher::default();
        let count = Arc::new(AtomicUsize::new(0));
        let (started, running) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let first = flusher.request(rt, move || {
            started.send(()).unwrap();
            released.recv().unwrap();
            Ok(())
        });
        std::thread::scope(|scope| {
            let worker = lock(&rt.0).take().unwrap();
            let worker = scope.spawn(worker);
            running.recv().unwrap();
            let later: Vec<_> = (0..8)
                .map(|_| {
                    let count = Arc::clone(&count);
                    flusher.request(rt, move || {
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                })
                .collect();
            assert!(lock(&rt.0).is_none());
            release.send(()).unwrap();
            crate::block_on(first).unwrap();
            for epoch in later {
                crate::block_on(epoch).unwrap();
            }
            worker.join().unwrap();
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_epoch_never_covers_a_later_request() {
        let rt = Manual::default();
        let flusher = Flusher::default();
        let first = flusher.request(&rt, || Err(io::Error::other("drive failed")));
        let same = flusher.request(&rt, || panic!("an epoch flushes once"));
        let worker = lock(&rt.0).take().unwrap();
        worker();
        assert!(crate::block_on(first).is_err());
        assert!(crate::block_on(same).is_err());
        let later = flusher.request(&rt, || Ok(()));
        let worker = lock(&rt.0).take().unwrap();
        worker();
        crate::block_on(later).unwrap();
    }

    #[test]
    fn dropping_a_waiter_does_not_cancel_other_sessions_promises() {
        let rt = Manual::default();
        let flusher = Flusher::default();
        drop(flusher.request(&rt, || Ok(())));
        let epoch = flusher.request(&rt, || panic!("the first request owns the flush"));
        let worker = lock(&rt.0).take().unwrap();
        worker();
        crate::block_on(epoch).unwrap();
    }
}
