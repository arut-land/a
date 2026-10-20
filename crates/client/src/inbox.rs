//! An app's inbox: what its [`Driver`](crate::Driver) and its input
//! threads post, taken by the one thread that folds it into the app.
//!
//! std's `mpsc` would do, but each type sent through it instantiates all
//! three of its channel flavours (about 7 KiB for the line client's, 15
//! for the terminal client's); a lock around a queue is all an inbox
//! needs, as it is only ever drained by one thread that sleeps when it is
//! empty.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

struct Queue<T> {
    /// Posted with [`Post::send_first`]: taken before `items`, in their
    /// own order.
    first: VecDeque<T>,
    items: VecDeque<T>,
    posts: usize,
    taken: bool,
}

impl<T> Queue<T> {
    fn pop(&mut self) -> Option<T> {
        self.first.pop_front().or_else(|| self.items.pop_front())
    }
}

struct Shared<T> {
    queue: Mutex<Queue<T>>,
    ready: Condvar,
}

impl<T> Shared<T> {
    // A panicking holder leaves the queue whole, so poisoning is ignored.
    fn queue(&self) -> MutexGuard<'_, Queue<T>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Posts to an [`Inbox`]; cheap to clone, one per producer.
pub struct Post<T>(Arc<Shared<T>>);

/// The receiving end: closed once every [`Post`] is dropped.
pub struct Inbox<T>(Arc<Shared<T>>);

pub fn inbox<T>() -> (Post<T>, Inbox<T>) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            first: VecDeque::new(),
            items: VecDeque::new(),
            posts: 1,
            taken: true,
        }),
        ready: Condvar::new(),
    });
    (Post(Arc::clone(&shared)), Inbox(shared))
}

impl<T> Post<T> {
    /// Queues `item`; `Err` gives it back once the inbox is gone.
    pub fn send(&self, item: T) -> Result<(), T> {
        self.post(item, false)
    }

    /// Queues `item` ahead of everything [`Post::send`] queued (after what
    /// was sent first before it): a person's keystroke is folded before
    /// the daemon's backlog, so a paste never waits behind a replay.
    pub fn send_first(&self, item: T) -> Result<(), T> {
        self.post(item, true)
    }

    fn post(&self, item: T, first: bool) -> Result<(), T> {
        let mut queue = self.0.queue();
        if !queue.taken {
            return Err(item);
        }
        if first {
            queue.first.push_back(item);
        } else {
            queue.items.push_back(item);
        }
        self.0.ready.notify_one();
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
            self.0.ready.notify_one();
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

    /// The next item already queued.
    pub fn try_recv(&self) -> Option<T> {
        self.0.queue().pop()
    }

    fn recv_by(&self, deadline: Option<Instant>) -> Option<T> {
        let mut queue = self.0.queue();
        loop {
            if let Some(item) = queue.pop() {
                return Some(item);
            }
            if queue.posts == 0 {
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
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::inbox;

    #[test]
    fn an_inbox_closes_when_its_last_post_goes() {
        let (post, inbox) = inbox();
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
        let (post, inbox) = inbox();
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
        let (post, inbox) = inbox();
        drop(inbox);
        assert_eq!(post.send(1), Err(1));
    }

    #[test]
    fn a_wait_ends_at_its_deadline() {
        let (_post, inbox) = inbox::<u8>();
        assert_eq!(inbox.recv_timeout(Duration::from_millis(5)), None);
        assert_eq!(inbox.try_recv(), None);
    }
}
