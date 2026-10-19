//! A session client behind a stream: what it tells the app goes out as
//! numbered changes, and the latest are kept so a client that missed some
//! can catch up without a full snapshot. The app renders the changes; every
//! rule (what runs, what waits, what the controls offer) comes computed.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use boltffi::{EventSubscription, StreamProducer, export};
use client::{Effect, Event, SessionClient, Welcome};
use protocol::{Caps, DaemonMsg, Decode};

use crate::{
    ClientError, Resync, SessionSnapshot, ToolOutcome, TurnInfo, ViewChange, ViewItem, ViewUpdate,
};

/// How many updates a stream buffers for a slow reader before it drops.
const BUFFER: usize = 1024;

/// How many recent updates [`Feed::resync`] can hand back, and about how
/// many bytes of text they may hold: a tool's outcome carries its output
/// twice (as JSON and as shown), so a long session's last few hundred
/// updates could otherwise keep tens of megabytes.
const KEPT: usize = 512;
const KEPT_BYTES: usize = 4 << 20;

pub(crate) struct Feed {
    pub client: SessionClient,
    /// The number of the last update published.
    rev: u64,
    recent: VecDeque<ViewUpdate>,
    /// What [`weight`] puts on `recent`.
    recent_bytes: usize,
    producer: StreamProducer<ViewUpdate>,
}

impl Feed {
    pub fn new(client: SessionClient) -> Self {
        Self {
            client,
            rev: 0,
            recent: VecDeque::with_capacity(KEPT),
            recent_bytes: 0,
            producer: StreamProducer::new(BUFFER),
        }
    }

    pub fn subscribe(&self) -> Arc<EventSubscription<ViewUpdate>> {
        self.producer.subscribe()
    }

    /// Steps the client with `event`, publishing what it tells the app;
    /// returns what the driver is to do.
    pub fn step(&mut self, event: Event) -> Vec<Effect> {
        let effects = self.client.step(event);
        self.absorb(effects)
    }

    /// Publishes the app's effects, handing back the driver's.
    pub fn absorb(&mut self, effects: Vec<Effect>) -> Vec<Effect> {
        let mut io = Vec::new();
        for effect in effects {
            let change = match effect {
                Effect::View(update) => ViewChange::of(&update.change, self.client.view()),
                Effect::Link => ViewChange::Link {
                    state: self.client.link().into(),
                },
                Effect::Notice(message) => ViewChange::Error { message },
                Effect::Sessions => continue,
                effect => {
                    io.push(effect);
                    continue;
                }
            };
            self.publish(change);
        }
        io
    }

    fn publish(&mut self, change: ViewChange) -> &ViewUpdate {
        self.rev += 1;
        let update = ViewUpdate {
            rev: self.rev,
            seq: self.client.view().seq,
            change,
            turn: self.client.turn().into(),
        };
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
        SessionSnapshot::of(&self.client, self.rev)
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
        ViewChange::Added { item, .. } | ViewChange::Updated { item, .. } => item_weight(item),
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

/// The session client on messages handed in as JSON, with no daemon: what
/// the bindings' tests run the shared vectors through, and a way to fold a
/// recorded session. It folds as if from a connection that is live.
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
        let mut client = SessionClient::new(client::Env::Keep);
        client.open(protocol::Target::New);
        client.step(Event::Connected {
            id: 1,
            welcome: Welcome {
                proto: protocol::PROTO,
                caps: Caps::ALL,
                client: 0,
            },
            remote: false,
            commands: Vec::new(),
        });
        Self {
            feed: Mutex::new(Feed::new(client)),
        }
    }

    /// Folds one daemon message, as the JSON the daemon frames it in. `None`
    /// when it changed nothing.
    pub fn fold(&self, message: &str) -> Result<Option<ViewUpdate>, ClientError> {
        let msg = DaemonMsg::from_json(message.as_bytes()).map_err(|e| ClientError::Protocol {
            message: e.to_string(),
        })?;
        let mut feed = lock(&self.feed);
        let effects = feed.client.step(Event::Msg { id: 1, msg });
        // As a view alone: no link to change.
        let shown = effects
            .into_iter()
            .filter(|e| matches!(e, Effect::View(_)))
            .collect();
        let rev = feed.rev;
        feed.absorb(shown);
        Ok((feed.rev > rev).then(|| feed.recent[feed.recent.len() - 1].clone()))
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
        serde_json::to_string(lock(&self.feed).client.view()).unwrap_or_default()
    }

    /// What the controls offer for the turn now.
    pub fn turn(&self) -> TurnInfo {
        lock(&self.feed).client.turn().into()
    }
}
