//! A cache flush covers barriers from many files. Requests arriving during
//! one syscall share the next epoch, which starts after all their writes.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::{Runtime, lock, oneshot};

type Work = Box<dyn FnOnce() -> io::Result<()> + Send>;

#[derive(Default)]
struct State {
    running: bool,
    work: Option<Work>,
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

impl Flusher {
    /// `work` must flush the same cache as every other request to this
    /// flusher. Only the first operation in each epoch needs to run.
    pub fn request(
        &self,
        rt: &dyn Runtime,
        work: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) -> FlushEpoch {
        let (send, receive) = oneshot::channel();
        let mut state = lock(&self.0);
        state.pending.push(send);
        if state.work.is_none() {
            state.work = Some(Box::new(work));
        }
        if !state.running {
            state.running = true;
            let shared = Arc::clone(&self.0);
            drop(state);
            rt.spawn_blocking(Box::new(move || run(&shared)));
        }
        FlushEpoch(receive)
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
