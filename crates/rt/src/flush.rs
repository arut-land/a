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
    finished: bool,
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
        leader.finished = true;
    }
}

impl Drop for FlushLeader {
    fn drop(&mut self) {
        if !self.finished {
            // Cancellation before admission must fail every enrolled epoch,
            // and leave this flusher usable by a subsequent runtime.
            let abandoned = std::mem::take(&mut *lock(&self.shared));
            drop(abandoned);
        }
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
            let admitted = rt.spawn_flush(Box::new(move || leader.run()));
            rt.spawn(Box::pin(async move {
                let _ = admitted.await;
            }));
        }
        self.epoch
    }
}

/// Returns storage admission before flushing, so one slow cache cannot stall writes.
pub fn unblock_flush<T: Send + 'static>(
    rt: &dyn Runtime,
    work: impl FnOnce() -> (T, Option<FlushLeader>) + Send + 'static,
) -> impl Future<Output = T> + Send + '_ {
    let (send, receive) = oneshot::channel();
    let admitted = rt.spawn_blocking(
        crate::BlockingClass::Storage,
        crate::STORAGE_BYTES,
        Box::new(move || {
            let (ordered, leader) = work();
            let leader = leader.map(FlushLeader::seal);
            send.send((ordered, leader));
        }),
    );
    async move {
        admitted.await.expect("write admission failed");
        let (ordered, leader) = receive.await.expect("ordered write panicked");
        if let Some(leader) = leader {
            let admitted = rt.spawn_flush(Box::new(move || leader.run()));
            rt.spawn(Box::pin(async move {
                let _ = admitted.await;
            }));
        }
        ordered
    }
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
                finished: false,
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
        fn spawn(&self, task: BoxFuture<'static, ()>) {
            crate::block_on(task);
        }
        fn spawn_blocking(
            &self,
            _: crate::BlockingClass,
            _: usize,
            work: Box<dyn FnOnce() + Send>,
        ) -> BoxFuture<'static, io::Result<()>> {
            assert!(
                lock(&self.0).replace(work).is_none(),
                "only one flush worker"
            );
            Box::pin(async { Ok(()) })
        }
        fn spawn_flush(
            &self,
            work: Box<dyn FnOnce() + Send>,
        ) -> BoxFuture<'static, io::Result<()>> {
            self.spawn_blocking(crate::BlockingClass::Storage, 0, work)
        }
        fn reactor(&self) -> Reactor {
            unreachable!()
        }
    }

    #[test]
    fn closed_admission_fails_every_enrolled_flush_waiter() {
        let runtime = crate::Threads::new(1);
        runtime.shutdown();
        let flusher = Flusher::default();
        let first = flusher.enroll(|| Ok(()));
        let second = flusher.enroll(|| Ok(()));
        assert!(crate::block_on(first.dispatch(&runtime)).is_err());
        assert!(crate::block_on(second.epoch).is_err());
        let next = crate::Threads::new(1);
        crate::block_on(flusher.request(&next, || Ok(()))).unwrap();
    }

    #[test]
    fn a_blocked_flush_never_holds_storage_admission() {
        let rt = crate::Threads::configured(crate::threads::Config::for_cores(1));
        let flusher = Flusher::default();
        let (started, running) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (value, epoch) = crate::block_on(unblock_flush(&rt, move || {
            let request = flusher.enroll(move || {
                started.send(()).unwrap();
                let _ = released.recv();
                Ok(())
            });
            ((7, request.epoch), request.leader)
        }));
        assert_eq!(value, 7);
        running
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let (done, received) = std::sync::mpsc::channel();
        let next = rt.spawn_blocking(
            crate::BlockingClass::Storage,
            0,
            Box::new(move || {
                let _ = done.send(());
            }),
        );
        rt.spawn(Box::pin(async move {
            let _ = next.await;
        }));
        let proceeded = received.recv_timeout(std::time::Duration::from_secs(1));
        release.send(()).unwrap();
        proceeded.unwrap();
        crate::block_on(epoch).unwrap();
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
