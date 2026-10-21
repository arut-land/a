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

use crate::{ChangeBatch, ClientError, Resync, SessionSnapshot, TITLE, TurnInfo, ViewChange};

/// How many updates a stream buffers for a slow reader before it drops.
const BUFFER: usize = 16;

/// How many recent updates [`Feed::resync`] can hand back. An item crosses
/// whole once and is patched after, so what they keep is about what the
/// transcript holds.
const BUDGET: client::QueueBudget = client::QueueBudget {
    bytes: 4 << 20,
    requests: 512,
    control_reserve: 0,
};

impl ChangeBatch {
    #[allow(unexpected_cfgs)]
    fn retained_bytes(&self) -> usize {
        #[cfg(not(boltffi_metadata))]
        {
            use boltffi::__private::wire::WireEncode;
            self.wire_size()
        }
        // The binding scanner omits generated codecs and never executes this code.
        #[cfg(boltffi_metadata)]
        {
            0
        }
    }
}

pub(crate) struct Feed {
    pub client: SessionClient,
    rev: u64,
    bytes: usize,
    pub retained_bytes: usize,
    recent: VecDeque<ChangeBatch>,
    producer: StreamProducer<ChangeBatch>,
}

impl Feed {
    pub fn new(client: SessionClient) -> Self {
        Self {
            client,
            rev: 0,
            bytes: 0,
            retained_bytes: 0,
            recent: VecDeque::with_capacity(BUDGET.requests),
            producer: StreamProducer::new(BUFFER),
        }
    }

    pub fn subscribe(&self) -> Arc<EventSubscription<ChangeBatch>> {
        self.producer.subscribe()
    }

    fn publish(&mut self, changes: Vec<ViewChange>) {
        self.rev += 1;
        let view = self.client.view();
        let mut update = ChangeBatch {
            revision: self.rev,
            seq: view.seq,
            changes,
            turn: self.client.turn().into(),
            streaming: view.streaming.as_ref().map(|s| s.call.get()),
            title: view.side(TITLE).map(str::to_owned),
        };
        let mut bytes = update.retained_bytes();
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        if bytes > BUDGET.bytes / BUFFER {
            self.recent.clear();
            self.bytes = 0;
            // The missing revision forces a snapshot before this small notification.
            self.rev += 1;
            update.revision = self.rev;
            update.changes = vec![ViewChange::Activity];
            update.title = None;
            bytes = update.retained_bytes();
        }
        self.producer.push(update.clone());
        while !BUDGET.admits(self.bytes + bytes, self.recent.len(), false) {
            if let Some(old) = self.recent.pop_front() {
                self.bytes -= old.retained_bytes();
            }
        }
        self.bytes += bytes;
        self.recent.push_back(update);
    }

    pub fn details(&self, key: u64) -> Option<String> {
        let items = &self.client.view().items;
        let at = items
            .binary_search_by_key(&key, |item| item.key().get())
            .ok()?;
        serde_json::to_string(&items[at]).ok()
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot::of(&self.client, self.rev)
    }

    /// The updates after rev `from`, or a snapshot once some are no longer
    /// kept.
    pub fn resync(&self, from: u64) -> Resync {
        let kept = from == self.rev
            || self
                .recent
                .front()
                .is_some_and(|u| u.revision <= from.saturating_add(1));
        if kept {
            Resync::Updates {
                updates: self
                    .recent
                    .iter()
                    .filter(|u| u.revision > from)
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

impl Renderer for Feed {
    fn show(&mut self, shown: Vec<Show>) -> Vec<Effect> {
        for show in shown {
            let change = match show {
                Show::View(update) => ViewChange::of(&update.change, self.client.view()),
                Show::Link => vec![ViewChange::Link {
                    state: self.client.link().into(),
                }],
                Show::Notice(notice) => vec![ViewChange::Error {
                    message: notice.to_string(),
                }],
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
    pub fn fold(&self, message: &str) -> Result<Option<ChangeBatch>, ClientError> {
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

    #[ffi_stream(item = ChangeBatch)]
    pub fn updates(&self) -> Arc<EventSubscription<ChangeBatch>> {
        lock(&self.feed).subscribe()
    }

    pub fn details(&self, key: u64) -> Option<String> {
        lock(&self.feed).details(key)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_updates_force_a_snapshot_instead_of_retaining_payload_copies() {
        let mut feed = Feed::new(SessionClient::new(client::Env::Keep));
        let stream = feed.subscribe();
        feed.publish(vec![ViewChange::Delta {
            call: 1,
            text: "x".repeat(BUDGET.bytes),
        }]);
        assert!(feed.bytes < BUDGET.bytes);
        assert!(matches!(feed.resync(0), Resync::Snapshot { .. }));
        let update = stream.pop_event().unwrap();
        assert_eq!(update.revision, 2);
        assert!(matches!(&update.changes[..], [ViewChange::Activity]));
        for _ in 0..BUDGET.requests * 2 {
            feed.publish(vec![ViewChange::Error {
                message: "x".repeat(16 << 10),
            }]);
        }
        assert!(feed.bytes <= BUDGET.bytes);
        assert!(feed.recent.len() <= BUDGET.requests);
        assert!(matches!(feed.resync(0), Resync::Snapshot { .. }));
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use protocol::{
        Annotation, AnnotationLifetime, ApprovalReason, Audience, Body, CallId, Card, Entry, Epoch,
        ExtensionFact, Json, Seq, Verdict,
    };

    fn fold(reducer: &Reducer, seq: u64, body: Body) -> Option<ChangeBatch> {
        let message = DaemonMsg::Entry(Entry::new(Seq::new(seq).unwrap(), Epoch(0), body));
        reducer
            .fold(&serde_json::to_string(&message).unwrap())
            .unwrap()
    }

    fn tool() -> Body {
        Body::ToolCall {
            invoke: None,
            tool: "custom".into(),
            input: Json::null(),
            id: None,
            card: Some(Card {
                title: "short title".into(),
                ..Card::default()
            }),
            by: Audience::Model,
        }
    }

    #[test]
    fn display_batches_apply_under_one_revision() {
        let reducer = Reducer::new();
        let added = fold(&reducer, 1, tool()).unwrap();
        let stream = reducer.updates();
        let batch = fold(
            &reducer,
            2,
            Body::Decision {
                by: "extension".into(),
                call: CallId::new(1),
                d: Verdict::PreTool {
                    input: Json::encode(&"effective input").unwrap(),
                },
            },
        )
        .unwrap();
        assert_eq!(batch.revision, added.revision + 1);
        assert!(matches!(
            &batch.changes[..],
            [
                ViewChange::Updated { index: 0, .. },
                ViewChange::Added { index: 1, .. }
            ]
        ));
        assert_eq!(stream.pop_event(), Some(batch.clone()));
        assert!(stream.pop_event().is_none());
        assert_eq!(
            reducer.resync(added.revision),
            Resync::Updates {
                updates: vec![batch]
            }
        );
    }

    #[test]
    fn small_changes_do_not_copy_the_transcript() {
        let size = |length| {
            let reducer = Reducer::new();
            for seq in 1..=length {
                fold(
                    &reducer,
                    seq,
                    Body::Assistant {
                        text: "transcript payload".repeat(1024),
                    },
                );
            }
            fold(&reducer, length + 1, tool());
            let batch = fold(
                &reducer,
                length + 2,
                Body::Decision {
                    by: "extension".into(),
                    call: CallId::new(length + 1),
                    d: Verdict::PreTool {
                        input: Json::null(),
                    },
                },
            )
            .unwrap();
            assert_eq!(batch.changes.len(), 2);
            batch.retained_bytes()
        };
        assert_eq!(size(1), size(100));
        assert!(size(1) < 1024);
    }

    #[test]
    fn approval_controls_follow_complete_effective_arguments() {
        let reducer = Reducer::new();
        fold(&reducer, 1, tool());
        let command = format!("{}\nremove sentinel", "x".repeat(16_384));
        let effective =
            Json::encode(&serde_json::json!({"command": command, "extra": {"flag": true}}))
                .unwrap();
        fold(
            &reducer,
            2,
            Body::Decision {
                by: "rewrite".into(),
                call: CallId::new(1),
                d: Verdict::PreTool {
                    input: effective.clone(),
                },
            },
        );
        fold(
            &reducer,
            3,
            Body::Decision {
                by: "policy".into(),
                call: CallId::new(1),
                d: Verdict::Permission {
                    permission: protocol::Permission::Ask(ApprovalReason::Mode {
                        mode: "ask".into(),
                    }),
                },
            },
        );
        fold(
            &reducer,
            4,
            Body::Fact {
                fact: ExtensionFact::Annotation(Annotation {
                    target: CallId::new(1).unwrap(),
                    lifetime: AnnotationLifetime::Question,
                    card: Card {
                        body: "Only lists files".into(),
                        ..Card::default()
                    },
                }),
            },
        );
        let snapshot = reducer.snapshot();
        let [
            crate::ApprovalPart::Call { text },
            crate::ApprovalPart::Reason { .. },
            crate::ApprovalPart::UnverifiedSummary { .. },
            crate::ApprovalPart::Controls { call: 1 },
        ] = &snapshot.items[0].presentation.approval[..]
        else {
            panic!("approval order: {:?}", snapshot.items[0]);
        };
        assert!(text.contains(&command));
        assert!(text.contains("flag"));
        assert!(reducer.details(1).unwrap().contains(effective.raw()));
        fold(
            &reducer,
            5,
            Body::Answer {
                call: CallId::new(1).unwrap(),
                value: Json::encode(&true).unwrap(),
                origin: None,
            },
        );
        assert_eq!(reducer.snapshot().items[0].presentation.approval, []);
    }
}
