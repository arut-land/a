//! A session client behind a stream: what it tells the app goes out as
//! numbered changes, and the latest are kept so a client that missed some
//! can catch up without a full snapshot. The app renders the changes; every
//! rule (what runs, what waits, what the controls offer, whose reply is
//! streaming) comes computed.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use boltffi::{EventSubscription, StreamProducer, export};
use client::{ConnId, Effect, Event, EventKind, Renderer, SessionClient, Show, Welcome};
use protocol::{Caps, DaemonMsg, Decode};

use crate::{ClientError, Resync, SessionSnapshot, TITLE, TurnInfo, ViewChange, ViewUpdate};

/// How many updates a stream buffers for a slow reader before it drops.
const BUFFER: usize = 1024;

/// How many recent updates [`Feed::resync`] can hand back. An item crosses
/// whole once and is patched after, so what they keep is about what the
/// transcript holds.
const KEPT: usize = 512;

pub(crate) struct Feed {
    pub client: SessionClient,
    /// The number of the last update published.
    rev: u64,
    recent: VecDeque<ViewUpdate>,
    producer: StreamProducer<ViewUpdate>,
}

impl Feed {
    pub fn new(client: SessionClient) -> Self {
        Self {
            client,
            rev: 0,
            recent: VecDeque::with_capacity(KEPT),
            producer: StreamProducer::new(BUFFER),
        }
    }

    pub fn subscribe(&self) -> Arc<EventSubscription<ViewUpdate>> {
        self.producer.subscribe()
    }

    fn publish(&mut self, change: ViewChange) {
        self.rev += 1;
        let view = self.client.view();
        let update = ViewUpdate {
            rev: self.rev,
            seq: view.seq,
            change,
            turn: self.client.turn().into(),
            streaming: view.streaming.as_ref().map(|s| s.call.get()),
            title: view.side(TITLE).map(str::to_owned),
        };
        self.producer.push(update.clone());
        if self.recent.len() == KEPT {
            self.recent.pop_front();
        }
        self.recent.push_back(update);
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

/// Publishes what the client shows.
impl Renderer for Feed {
    fn show(&mut self, shown: Vec<Show>) -> Vec<Effect> {
        for show in shown {
            let change = match show {
                Show::View(update) => ViewChange::of(&update.change, self.client.view()),
                Show::Link => ViewChange::Link {
                    state: self.client.link().into(),
                },
                Show::Notice(notice) => ViewChange::Error {
                    message: notice.to_string(),
                },
                Show::Sessions => continue,
            };
            self.publish(change);
        }
        Vec::new()
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
        client.step(Event {
            conn: ConnId(1),
            kind: EventKind::Connected {
                welcome: Welcome {
                    proto: protocol::PROTO,
                    caps: Caps::ALL,
                },
                remote: false,
                commands: Vec::new(),
            },
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
        let effects = feed.client.step(Event {
            conn: ConnId(1),
            kind: EventKind::Msg(msg),
        });
        // As a view alone: no link to change.
        let shown = effects
            .into_iter()
            .filter(|e| matches!(e, Effect::Show(Show::View(_))))
            .collect();
        let rev = feed.rev;
        client::pump(|_| Ok(()), &mut *feed, shown);
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
