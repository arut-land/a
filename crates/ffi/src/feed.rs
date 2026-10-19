//! The shared reducer behind a stream: folds daemon messages, numbers the
//! updates, and keeps the latest so a client that missed some can catch up
//! without a full snapshot.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use boltffi::{EventSubscription, StreamProducer, export};
use protocol::view::SessionView;
use protocol::{DaemonMsg, Decode};

use crate::{ClientError, Resync, SessionSnapshot, ToolOutcome, ViewChange, ViewItem, ViewUpdate};

/// How many updates a stream buffers for a slow reader before it drops.
const BUFFER: usize = 1024;

/// How many recent updates [`Feed::resync`] can hand back, and about how
/// many bytes of text they may hold: a tool's outcome carries its output
/// twice (as JSON and as shown), so a long session's last few hundred
/// updates could otherwise keep tens of megabytes.
const KEPT: usize = 512;
const KEPT_BYTES: usize = 4 << 20;

pub(crate) struct Feed {
    pub view: SessionView,
    recent: VecDeque<ViewUpdate>,
    /// What [`weight`] puts on `recent`.
    recent_bytes: usize,
    /// The connection ended for good.
    pub closed: bool,
    producer: StreamProducer<ViewUpdate>,
}

impl Feed {
    pub fn new() -> Self {
        Self {
            view: SessionView::default(),
            recent: VecDeque::with_capacity(KEPT),
            recent_bytes: 0,
            closed: false,
            producer: StreamProducer::new(BUFFER),
        }
    }

    pub fn subscribe(&self) -> Arc<EventSubscription<ViewUpdate>> {
        self.producer.subscribe()
    }

    /// Folds `msg` and publishes what it changed.
    pub fn fold(&mut self, msg: &DaemonMsg) -> Option<&ViewUpdate> {
        let update = ViewUpdate::from(&self.view.fold(msg)?);
        Some(self.publish(update))
    }

    /// Publishes a change of this facade's own, numbered in the view's
    /// sequence.
    pub fn local(&mut self, change: ViewChange) {
        self.view.rev += 1;
        let update = ViewUpdate {
            rev: self.view.rev,
            seq: self.view.seq,
            change,
        };
        self.publish(update);
    }

    fn publish(&mut self, update: ViewUpdate) -> &ViewUpdate {
        self.producer.push(update.clone());
        self.recent_bytes += weight(&update.change);
        self.recent.push_back(update);
        while self.recent.len() > KEPT || (self.recent.len() > 1 && self.recent_bytes > KEPT_BYTES)
        {
            if let Some(old) = self.recent.pop_front() {
                self.recent_bytes -= weight(&old.change);
            }
        }
        &self.recent[self.recent.len() - 1]
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

/// About how many bytes of text `change` holds.
fn weight(change: &ViewChange) -> usize {
    match change {
        ViewChange::Added { item, .. }
        | ViewChange::Updated { item, .. }
        | ViewChange::Asked { item, .. } => item_weight(item),
        ViewChange::Snapshot { items } => items.iter().map(item_weight).sum(),
        ViewChange::Delta { text, .. } => text.len(),
        _ => 0,
    }
}

fn item_weight(item: &ViewItem) -> usize {
    let outcome = |o: &Option<ToolOutcome>| match o {
        Some(ToolOutcome::Done { output_json, shown }) => {
            output_json.len() + shown.as_ref().map_or(0, |s| s.text.len())
        }
        Some(ToolOutcome::Failed { error }) => error.len(),
        _ => 0,
    };
    match item {
        ViewItem::User { text, .. }
        | ViewItem::Assistant { text, .. }
        | ViewItem::Reply { text, .. } => text.len(),
        ViewItem::Tool {
            input_json,
            outcome: o,
            card,
            ..
        } => input_json.len() + outcome(o) + card.as_ref().map_or(0, |c| c.body.len()),
        ViewItem::Command { outcome: o, .. } | ViewItem::Child { fired: o, .. } => outcome(o),
        ViewItem::Hook { decision_json, .. } => decision_json.len(),
        ViewItem::Environment { .. } => 0,
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
        Ok(lock(&self.feed).fold(&msg).cloned())
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
