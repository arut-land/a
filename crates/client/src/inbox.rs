//! The app's bounded inbox for daemon events and input.
//! A locked queue avoids std mpsc's per-type channel implementations,
//! which cost 7–15 KiB in the shipped clients.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

pub use protocol::QueueBudget;

pub const PUMP_BATCH: usize = 64;

struct Queue<T> {
    /// Posted with [`Post::send_first`]: taken before `items`, in their
    /// own order.
    first: VecDeque<(usize, T)>,
    items: VecDeque<(usize, T)>,
    posts: usize,
    bytes: usize,
    first_bytes: usize,
    ended: bool,
    waker: Option<Waker>,
    taken: bool,
}

impl<T> Queue<T> {
    fn pop(&mut self) -> Option<T> {
        if let Some((bytes, item)) = self.first.pop_front() {
            self.first_bytes -= bytes;
            return Some(item);
        }
        let (bytes, item) = self.items.pop_front()?;
        self.bytes -= bytes;
        Some(item)
    }
}

struct Shared<T> {
    queue: Mutex<Queue<T>>,
    ready: Condvar,
    space: rt::Notify,
    budget: QueueBudget,
    weight: fn(&T) -> usize,
}

impl<T> Shared<T> {
    fn wake(&self, queue: &mut Queue<T>) {
        if let Some(waker) = queue.waker.take() {
            waker.wake();
        }
        self.ready.notify_one();
    }

    // A panicking holder leaves the queue whole, so poisoning is ignored.
    fn queue(&self) -> MutexGuard<'_, Queue<T>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Posts to an [`Inbox`]; cheap to clone, one per producer.
pub struct Post<T>(Arc<Shared<T>>);

/// The receiving end: closed once every [`Post`] is dropped.
pub struct Inbox<T>(Arc<Shared<T>>);

pub fn inbox<T>(budget: QueueBudget, weight: fn(&T) -> usize) -> (Post<T>, Inbox<T>) {
    let shared = Arc::new(Shared {
        budget,
        weight,
        queue: Mutex::new(Queue {
            first: VecDeque::new(),
            items: VecDeque::new(),
            posts: 1,
            bytes: 0,
            first_bytes: 0,
            ended: false,
            waker: None,
            taken: true,
        }),
        ready: Condvar::new(),
        space: rt::Notify::new(),
    });
    (Post(Arc::clone(&shared)), Inbox(shared))
}

#[derive(Clone, Copy)]
enum Lane {
    Control,
    Data,
    Stream,
}

impl<T> Post<T> {
    /// Waits for space without blocking a runtime worker. One oversized item
    /// may occupy an otherwise empty data lane, so budgets never truncate history.
    pub async fn send_async(&self, mut item: T) -> Result<(), T>
    where
        T: Send,
    {
        loop {
            let seen = self.0.space.count();
            item = match self.post(item, Lane::Stream) {
                Ok(()) => return Ok(()),
                Err(item) => item,
            };
            {
                let queue = self.0.queue();
                if !queue.taken || queue.ended {
                    return Err(item);
                }
            }
            self.0.space.until(|| self.0.space.count() != seen).await;
        }
    }

    /// Returns `item` if the byte/request budget is full or the receiver is gone.
    pub fn send(&self, item: T) -> Result<(), T> {
        self.post(item, Lane::Data)
    }

    /// Reserves priority capacity so keystrokes stay ahead of replay.
    pub fn send_first(&self, item: T) -> Result<(), T> {
        self.post(item, Lane::Control)
    }

    /// Replaces an incomplete backlog with an explicit terminal outcome.
    pub fn close_with(&self, item: T) {
        let mut queue = self.0.queue();
        queue.first.clear();
        queue.items.clear();
        queue.bytes = 0;
        queue.first_bytes = 0;
        queue.ended = true;
        queue.first.push_back((0, item));
        self.0.wake(&mut queue);
        self.0.space.notify();
    }

    fn post(&self, item: T, lane: Lane) -> Result<(), T> {
        let mut queue = self.0.queue();
        let bytes = (self.0.weight)(&item);
        let admitted = if matches!(lane, Lane::Control) {
            self.0.budget.admits(
                queue.first_bytes.saturating_add(bytes),
                queue.first.len(),
                true,
            )
        } else {
            self.0
                .budget
                .admits(queue.bytes.saturating_add(bytes), queue.items.len(), false)
                || matches!(lane, Lane::Stream) && queue.items.is_empty()
        };
        if !queue.taken || queue.ended || !admitted {
            return Err(item);
        }
        if matches!(lane, Lane::Control) {
            queue.first.push_back((bytes, item));
            queue.first_bytes += bytes;
        } else {
            queue.items.push_back((bytes, item));
            queue.bytes += bytes;
        }
        self.0.wake(&mut queue);
        Ok(())
    }
}

impl<T> Clone for Post<T> {
    fn clone(&self) -> Self {
        self.0.queue().posts += 1;
        Self(Arc::clone(&self.0))
    }
}

impl<T> Drop for Post<T> {
    fn drop(&mut self) {
        let mut queue = self.0.queue();
        queue.posts -= 1;
        if queue.posts == 0 {
            self.0.wake(&mut queue);
        }
    }
}

impl<T> Inbox<T> {
    /// The next item, waiting for one; `None` once nothing is queued and
    /// nothing can post.
    pub fn recv(&self) -> Option<T> {
        self.recv_by(None)
    }

    /// [`Inbox::recv`], waiting no longer than `within`.
    pub fn recv_timeout(&self, within: Duration) -> Option<T> {
        self.recv_by(Some(Instant::now() + within))
    }

    /// Nothing is queued and nothing can post.
    pub fn closed(&self) -> bool {
        let queue = self.0.queue();
        (queue.posts == 0 || queue.ended) && queue.first.is_empty() && queue.items.is_empty()
    }

    /// The next item already queued.
    pub fn try_recv(&self) -> Option<T> {
        let item = self.0.queue().pop();
        if item.is_some() {
            self.0.space.notify();
        }
        item
    }

    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut queue = self.0.queue();
        if let Some(item) = queue.pop() {
            self.0.space.notify();
            return Poll::Ready(Some(item));
        }
        if queue.posts == 0 || queue.ended {
            return Poll::Ready(None);
        }
        queue.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn recv_by(&self, deadline: Option<Instant>) -> Option<T> {
        let mut queue = self.0.queue();
        loop {
            if let Some(item) = queue.pop() {
                self.0.space.notify();
                return Some(item);
            }
            if queue.posts == 0 || queue.ended {
                return None;
            }
            let ready = &self.0.ready;
            queue = match deadline {
                None => ready.wait(queue).unwrap_or_else(PoisonError::into_inner),
                Some(deadline) => {
                    let left = deadline.checked_duration_since(Instant::now())?;
                    let waited = ready.wait_timeout(queue, left);
                    waited.unwrap_or_else(PoisonError::into_inner).0
                }
            };
        }
    }
}

impl<T> Drop for Inbox<T> {
    fn drop(&mut self) {
        let mut queue = self.0.queue();
        queue.taken = false;
        queue.first.clear();
        queue.items.clear();
        self.0.ready.notify_all();
        self.0.space.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::{QueueBudget, inbox};
    use std::task::Poll;

    #[test]
    fn durable_backpressure_preserves_order_and_reserves_control_capacity() {
        use std::future::Future;
        use std::pin::pin;
        use std::task::{Context, Waker};

        for budget in [
            QueueBudget {
                bytes: 8192,
                requests: 2,
                control_reserve: 1,
            },
            QueueBudget {
                bytes: 4097,
                requests: 100,
                control_reserve: 1,
            },
        ] {
            let (post, inbox) = inbox(budget, String::len);
            post.send("a".into()).unwrap();
            let mut pending = pin!(post.send_async("b".into()));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(pending.as_mut().poll(&mut cx).is_pending());
            post.send_first("interrupt".into()).unwrap();
            assert_eq!(inbox.try_recv().as_deref(), Some("interrupt"));
            assert!(pending.as_mut().poll(&mut cx).is_pending());
            assert_eq!(inbox.try_recv().as_deref(), Some("a"));
            assert!(matches!(
                pending.as_mut().poll(&mut cx),
                Poll::Ready(Ok(()))
            ));
            assert_eq!(inbox.try_recv().as_deref(), Some("b"));
        }
    }

    #[test]
    fn an_oversized_event_is_delivered_without_starving_control() {
        let (post, inbox) = inbox(
            QueueBudget {
                bytes: 8192,
                requests: 2,
                control_reserve: 1,
            },
            String::len,
        );
        rt::block_on(post.send_async("x".repeat(16384))).unwrap();
        post.send_first("interrupt".into()).unwrap();
        assert_eq!(inbox.try_recv().as_deref(), Some("interrupt"));
        assert_eq!(inbox.try_recv().unwrap().len(), 16384);
    }

    #[test]
    fn dropping_the_consumer_wakes_a_backpressured_producer() {
        use rt::Runtime;
        use std::sync::mpsc;

        let (post, inbox) = inbox(
            QueueBudget {
                bytes: 8192,
                requests: 2,
                control_reserve: 1,
            },
            |_| 1,
        );
        post.send(1).unwrap();
        let runtime = rt::Threads::new(1);
        let (sent, received) = mpsc::channel();
        let (waiting, pending) = mpsc::channel();
        runtime.spawn(Box::pin(async move {
            use std::future::{Future, poll_fn};
            use std::pin::pin;

            let mut send = pin!(post.send_async(2));
            poll_fn(|cx| {
                assert!(send.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            waiting.send(()).unwrap();
            sent.send(send.await).unwrap();
        }));
        pending.recv_timeout(Duration::from_secs(2)).unwrap();
        // A second task must run even while the producer awaits space.
        let (progress, progressed) = mpsc::channel();
        runtime.spawn(Box::pin(async move {
            progress.send(()).unwrap();
        }));
        progressed.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(inbox);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(2)
        );
    }

    #[test]
    fn byte_overload_returns_the_payload_and_a_terminal_outcome_is_observable() {
        let (post, inbox) = inbox(
            QueueBudget {
                bytes: 64 << 10,
                requests: 4,
                control_reserve: 1,
            },
            String::len,
        );
        post.send("x".repeat(32 << 10)).unwrap();
        let refused = "y".repeat(32 << 10);
        assert_eq!(post.send(refused.clone()), Err(refused));
        post.send_first("interrupt".into()).unwrap();
        assert_eq!(inbox.try_recv().as_deref(), Some("interrupt"));
        post.close_with("resync required".into());
        assert_eq!(inbox.recv().as_deref(), Some("resync required"));
        assert_eq!(inbox.recv(), None);
        assert!(inbox.closed());
    }

    #[test]
    fn an_inbox_closes_when_its_last_post_goes() {
        let (post, inbox) = inbox(QueueBudget::default(), |_| size_of::<i32>());
        let other = post.clone();
        thread::spawn(move || {
            post.send(1).unwrap();
            other.send(2).unwrap();
        });
        assert_eq!(inbox.recv(), Some(1));
        assert_eq!(inbox.recv(), Some(2));
        assert_eq!(inbox.recv(), None);
    }

    #[test]
    fn what_is_posted_first_is_taken_before_the_backlog_in_its_own_order() {
        let (post, inbox) = inbox(
            QueueBudget {
                control_reserve: 1,
                ..QueueBudget::default()
            },
            |_| size_of::<i32>(),
        );
        for n in 1..=3 {
            post.send(n).unwrap();
        }
        post.send_first(10).unwrap();
        post.send_first(11).unwrap();
        let taken: Vec<_> = std::iter::from_fn(|| inbox.try_recv()).collect();
        assert_eq!(taken, [10, 11, 1, 2, 3]);
    }

    #[test]
    fn a_post_to_a_dropped_inbox_comes_back() {
        let (post, inbox) = inbox(QueueBudget::default(), |_| size_of::<i32>());
        drop(inbox);
        assert_eq!(post.send(1), Err(1));
    }

    #[test]
    fn a_wait_ends_at_its_deadline() {
        let (_post, inbox) = inbox::<u8>(QueueBudget::default(), |_| 1);
        assert_eq!(inbox.recv_timeout(Duration::from_millis(5)), None);
        assert_eq!(inbox.try_recv(), None);
    }
}
