//! The shared reducer behind a stream: folds daemon messages, numbers the
//! updates, and keeps the latest so a client that missed some can catch up
//! without a full snapshot.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use boltffi::{EventSubscription, StreamProducer, export};
use protocol::view::SessionView;
use protocol::{DaemonMsg, Decode};

use crate::{ClientError, Resync, SessionSnapshot, ViewChange, ViewUpdate};

/// How many updates a stream buffers for a slow reader before it drops.
const BUFFER: usize = 1024;

/// How many recent updates [`Feed::resync`] can hand back.
const KEPT: usize = 512;

pub(crate) struct Feed {
    pub view: SessionView,
    recent: VecDeque<ViewUpdate>,
    /// The connection ended for good.
    pub closed: bool,
    producer: StreamProducer<ViewUpdate>,
}

impl Feed {
    pub fn new() -> Self {
        Self {
            view: SessionView::default(),
            recent: VecDeque::with_capacity(KEPT),
            closed: false,
            producer: StreamProducer::new(BUFFER),
        }
    }

    pub fn subscribe(&self) -> Arc<EventSubscription<ViewUpdate>> {
        self.producer.subscribe()
    }

    /// Folds `msg` and publishes what it changed.
    pub fn fold(&mut self, msg: &DaemonMsg) -> Option<ViewUpdate> {
        let update = ViewUpdate::from(&self.view.fold(msg)?);
        Some(self.publish(update))
    }

    /// Publishes a change of this facade's own, numbered in the view's
    /// sequence.
    pub fn local(&mut self, change: ViewChange) -> ViewUpdate {
        self.view.rev += 1;
        let update = ViewUpdate {
            rev: self.view.rev,
            seq: self.view.seq,
            change,
        };
        self.publish(update)
    }

    fn publish(&mut self, update: ViewUpdate) -> ViewUpdate {
        if self.recent.len() == KEPT {
            self.recent.pop_front();
        }
        self.recent.push_back(update.clone());
        self.producer.push(update.clone());
        update
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot::of(&self.view, self.closed)
    }

    /// The updates after rev `from`, or a snapshot once some are no longer
    /// kept.
    pub fn resync(&self, from: u64) -> Resync {
        let kept = self.recent.front().is_none_or(|u| u.rev <= from + 1);
        if kept {
            Resync::Updates {
                updates: self
                    .recent
                    .iter()
                    .filter(|u| u.rev > from)
                    .cloned()
                    .collect(),
            }
        } else {
            Resync::Snapshot {
                snapshot: self.snapshot(),
            }
        }
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The shared reducer on messages handed in as JSON, with no daemon: what
/// the bindings' tests run the shared vectors through, and a way to fold a
/// recorded session.
pub struct Reducer {
    feed: Mutex<Feed>,
}

impl Default for Reducer {
    fn default() -> Self {
        Self::new()
    }
}

#[export]
impl Reducer {
    pub fn new() -> Self {
        Self {
            feed: Mutex::new(Feed::new()),
        }
    }

    /// Folds one daemon message, as the JSON the daemon frames it in. `None`
    /// when it changed nothing.
    pub fn fold(&self, message: &str) -> Result<Option<ViewUpdate>, ClientError> {
        let msg = DaemonMsg::from_json(message.as_bytes()).map_err(|e| ClientError::Protocol {
            message: e.to_string(),
        })?;
        Ok(lock(&self.feed).fold(&msg))
    }

    #[ffi_stream(item = ViewUpdate)]
    pub fn updates(&self) -> Arc<EventSubscription<ViewUpdate>> {
        lock(&self.feed).subscribe()
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        lock(&self.feed).snapshot()
    }

    pub fn resync(&self, from: u64) -> Resync {
        lock(&self.feed).resync(from)
    }

    /// The log-derived state as JSON, the form the vectors' `state` is in.
    pub fn state_json(&self) -> String {
        serde_json::to_string(&lock(&self.feed).view).unwrap_or_default()
    }
}
