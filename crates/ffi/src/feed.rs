//! A session client behind a stream: what it tells the app goes out as
//! numbered changes, and the latest are kept so a client that missed some
//! can catch up without a full snapshot. The app renders the changes; every
//! rule (what runs, what waits, what the controls offer, whose reply is
//! streaming) comes computed.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use boltffi::{EventSubscription, StreamProducer, export};
use client::{ConnId, Effect, Event, EventKind, Renderer, SessionClient, Show, Welcome};
use protocol::{Caps, DaemonMsg, Decode};

use crate::{
    ChangeBatch, ClientError, LinkState, Resync, SessionSnapshot, StreamingText, TITLE, TurnInfo,
    ViewChange,
};

/// How many updates a stream buffers for a slow reader before it drops.
const BUFFER: usize = 16;

/// Disposable replay data: large presentations stay in the current view
/// and cross only when a reader recovers their invalidations.
const BUDGET: client::QueueBudget = client::QueueBudget {
    bytes: 4 << 20,
    requests: 512,
    control_reserve: 0,
};

macro_rules! retained_bytes {
    ($ty:ty) => {
        impl $ty {
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
    };
}

retained_bytes!(ChangeBatch);
retained_bytes!(crate::DisplayItem);

#[derive(Default)]
struct JsonBytes(usize);

impl Write for JsonBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

macro_rules! json_bytes {
    ($value:expr) => {{
        let mut count = JsonBytes::default();
        serde_json::to_writer(&mut count, $value).expect("view payload serializes");
        count.0
    }};
}

pub(crate) struct Feed {
    pub client: SessionClient,
    rev: u64,
    bytes: usize,
    // Raw transcript items and the display payload Swift retains are distinct.
    item_bytes: Vec<usize>,
    payload_bytes: usize,
    state_bytes: usize,
    state_seq: u64,
    recent: VecDeque<ChangeBatch>,
    producer: StreamProducer<ChangeBatch>,
}

impl Feed {
    pub fn new(client: SessionClient) -> Self {
        Self {
            rev: 0,
            bytes: 0,
            item_bytes: Vec::new(),
            payload_bytes: 0,
            state_bytes: json_bytes!(&client.view().state),
            state_seq: client.view().seq,
            recent: VecDeque::with_capacity(BUDGET.requests),
            producer: StreamProducer::new(BUFFER),
            client,
        }
    }

    pub fn step(&mut self, event: Event) -> Vec<Effect> {
        let reset = matches!(
            &event.kind,
            EventKind::Msg(DaemonMsg::Snapshot(_) | DaemonMsg::Attached { .. })
        );
        let effects = self.client.step(event);
        let view = self.client.view();
        if self.state_seq != view.seq || reset {
            self.state_bytes = json_bytes!(&view.state);
            self.state_seq = view.seq;
        }
        effects
    }

    pub fn subscribe(&self) -> Arc<EventSubscription<ChangeBatch>> {
        self.producer.subscribe()
    }

    fn batch(&mut self, changes: Vec<ViewChange>) -> ChangeBatch {
        for change in &changes {
            match change {
                ViewChange::Attached { reset: true, .. } => {
                    self.item_bytes.clear();
                    self.payload_bytes = 0;
                }
                ViewChange::Snapshot { items, .. } => {
                    self.item_bytes = self
                        .client
                        .view()
                        .items
                        .iter()
                        .zip(items)
                        .map(|(raw, display)| json_bytes!(raw) + display.retained_bytes())
                        .collect();
                    self.payload_bytes = self.item_bytes.iter().sum();
                }
                ViewChange::Added {
                    index,
                    item: display,
                }
                | ViewChange::Updated {
                    index,
                    item: display,
                } => {
                    let index = *index as usize;
                    if let Some(item) = self.client.view().items.get(index) {
                        let bytes = json_bytes!(item) + display.retained_bytes();
                        if matches!(change, ViewChange::Updated { .. }) {
                            self.payload_bytes -= self.item_bytes[index];
                            self.item_bytes[index] = bytes;
                        } else {
                            self.item_bytes.insert(index, bytes);
                        }
                        self.payload_bytes += bytes;
                    }
                }
                _ => {}
            }
        }
        if changes
            .iter()
            .any(|change| matches!(change, ViewChange::Snapshot { .. }))
        {
            // Recovery reads the current window; retaining displaced windows duplicates history.
            self.recent.clear();
            self.bytes = 0;
        }
        self.rev += 1;
        let mut update = self.update(changes);
        // View data can be recovered; notices still cross intact.
        if update.retained_bytes() > BUDGET.bytes / BUFFER
            && !update.changes.iter().any(|change| {
                matches!(
                    change,
                    ViewChange::Error { .. }
                        | ViewChange::Mismatch { .. }
                        | ViewChange::Link { .. }
                )
            })
        {
            let mut added = Vec::new();
            let mut updated = Vec::new();
            let replaced = update
                .changes
                .iter()
                .any(|change| matches!(change, ViewChange::Snapshot { .. }));
            update.changes.retain(|change| match change {
                ViewChange::Added { index, .. } => {
                    added.push(*index);
                    false
                }
                ViewChange::Updated { index, .. } => {
                    updated.push(*index);
                    false
                }
                ViewChange::Delta { .. } | ViewChange::Snapshot { .. } => false,
                _ => true,
            });
            update.changes.push(if replaced {
                ViewChange::ViewInvalidated
            } else {
                ViewChange::Invalidated { added, updated }
            });
            update.title = None;
        }
        let bytes = update.retained_bytes();
        while !BUDGET.admits(self.bytes + bytes, self.recent.len(), false) {
            let Some(old) = self.recent.pop_front() else {
                break;
            };
            self.bytes -= old.retained_bytes();
        }
        let retain = BUDGET.admits(bytes, 0, false);
        if retain {
            self.bytes += bytes;
        }
        update.retained_bytes = self.retained_bytes() as u64;
        if retain {
            self.recent.push_back(update.clone());
        }
        update
    }

    fn update(&self, changes: Vec<ViewChange>) -> ChangeBatch {
        let view = self.client.view();
        ChangeBatch {
            history: crate::HistoryWindow::of(&self.client),
            revision: self.rev,
            retained_bytes: self.retained_bytes() as u64,
            seq: view.seq,
            changes,
            turn: self.client.turn().into(),
            streaming: view.streaming.as_ref().map(|s| crate::StreamId {
                call: s.call.get(),
                attempt: s.attempt,
            }),
            title: view.side(TITLE).map(str::to_owned),
        }
    }

    fn render(&mut self, show: Show) -> Option<ChangeBatch> {
        let changes = match show {
            Show::View(update) => ViewChange::of(&update.change, self.client.view()),
            Show::Link => vec![ViewChange::Link {
                state: self.client.link().into(),
            }],
            Show::Notice(notice) => vec![ViewChange::Error {
                message: notice.to_string(),
            }],
            Show::Sessions | Show::History { .. } => return None,
        };
        Some(self.batch(changes))
    }

    pub fn retained_bytes(&self) -> usize {
        self.payload_bytes
            + self.state_bytes
            + self.bytes
            + self
                .client
                .view()
                .streaming
                .as_ref()
                .map_or(0, |s| s.text.len() * 2)
    }

    pub fn details(&self, key: u64) -> Option<String> {
        let items = &self.client.view().items;
        let at = items
            .binary_search_by_key(&key, |item| item.key().get())
            .ok()?;
        serde_json::to_string(&items[at]).ok()
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot::of(&self.client, self.rev, self.retained_bytes() as u64)
    }

    /// Replays retained changes, coalescing invalidations against `from`.
    /// A lost replay range or replaced view requires a snapshot.
    #[cfg(test)]
    pub fn resync(&self, from: u64) -> Resync {
        self.prepare(from)
            .render(&AtomicBool::new(false), || {})
            .unwrap()
    }

    fn prepare(&self, from: u64) -> PreparedResync {
        let kept = from == self.rev
            || self
                .recent
                .front()
                .is_some_and(|u| u.revision <= from.saturating_add(1));
        if !kept {
            return self.prepare_snapshot();
        }
        let updates = || self.recent.iter().filter(|update| update.revision > from);
        if !updates().any(|update| {
            update.changes.iter().any(|change| {
                matches!(
                    change,
                    ViewChange::Invalidated { .. } | ViewChange::ViewInvalidated
                )
            })
        }) {
            return PreparedResync::Ready(Resync::Updates {
                updates: updates().cloned().collect(),
            });
        }
        let mut items = BTreeMap::new();
        let mut changes = Vec::new();
        for change in updates().flat_map(|update| &update.changes) {
            match change {
                ViewChange::Snapshot { .. }
                | ViewChange::ViewInvalidated
                | ViewChange::Attached { reset: true, .. } => {
                    return self.prepare_snapshot();
                }
                ViewChange::Added { index, .. } => {
                    items.insert(*index, true);
                }
                ViewChange::Updated { index, .. } => {
                    items.entry(*index).or_insert(false);
                }
                ViewChange::Invalidated { added, updated } => {
                    for index in added {
                        items.insert(*index, true);
                    }
                    for index in updated {
                        items.entry(*index).or_insert(false);
                    }
                }
                ViewChange::Delta { .. } => {}
                _ => changes.push(change.clone()),
            }
        }
        let view = self.client.view();
        // Items only append between the view replacements rejected above.
        let items = items
            .into_iter()
            .map(|(index, added)| (index, added, view.items[index as usize].clone()))
            .collect();
        let mut presentation = protocol::view::SessionView::default();
        presentation.state = view.state.clone();
        PreparedResync::Patch {
            from,
            update: self.update(changes),
            presentation: Box::new(presentation),
            items,
            streaming: view.streaming.as_ref().map(|stream| StreamingText {
                call: stream.call.get(),
                attempt: stream.attempt,
                text: stream.text.clone(),
            }),
        }
    }

    fn prepare_snapshot(&self) -> PreparedResync {
        PreparedResync::Snapshot {
            view: Box::new(self.client.view().clone()),
            rev: self.rev,
            retained_bytes: self.retained_bytes() as u64,
            link: self.client.link().into(),
            turn: self.client.turn().into(),
            history: crate::HistoryWindow::of(&self.client),
        }
    }
}

/// An owned revision, captured under the feed lock and rendered without it.
enum PreparedResync {
    Ready(Resync),
    Snapshot {
        view: Box<protocol::view::SessionView>,
        rev: u64,
        retained_bytes: u64,
        link: LinkState,
        turn: TurnInfo,
        history: Option<crate::HistoryWindow>,
    },
    Patch {
        from: u64,
        update: ChangeBatch,
        presentation: Box<protocol::view::SessionView>,
        items: Vec<(u32, bool, protocol::view::Item)>,
        streaming: Option<StreamingText>,
    },
}

impl PreparedResync {
    fn render(self, cancelled: &AtomicBool, mut rendered: impl FnMut()) -> Option<Resync> {
        let result = match self {
            Self::Ready(result) => result,
            Self::Snapshot {
                view,
                rev,
                retained_bytes,
                link,
                turn,
                history,
            } => {
                let mut items = Vec::with_capacity(view.items.len());
                for item in &view.items {
                    if cancelled.load(Ordering::Relaxed) {
                        return None;
                    }
                    items.push(crate::DisplayItem::in_view(item, &view));
                    rendered();
                }
                if cancelled.load(Ordering::Relaxed) {
                    return None;
                }
                Resync::Snapshot {
                    snapshot: SessionSnapshot::with_items(
                        &view,
                        rev,
                        retained_bytes,
                        link,
                        turn,
                        history,
                        items,
                    ),
                }
            }
            Self::Patch {
                from,
                mut update,
                presentation,
                items,
                streaming,
            } => {
                for (index, added, raw) in items {
                    if cancelled.load(Ordering::Relaxed) {
                        return None;
                    }
                    let item = crate::DisplayItem::in_view(&raw, &presentation);
                    update.changes.push(if added {
                        ViewChange::Added { index, item }
                    } else {
                        ViewChange::Updated { index, item }
                    });
                    rendered();
                }
                Resync::Patch {
                    from,
                    update,
                    streaming,
                }
            }
        };
        (!cancelled.load(Ordering::Relaxed)).then_some(result)
    }
}

/// One cancellable recovery build. Cancellation also covers a worker waiting
/// for the feed lock; rendering owns its captured revision after that lock is released.
pub struct Recovery {
    feed: Arc<Mutex<Feed>>,
    from: u64,
    cancelled: AtomicBool,
}

impl Recovery {
    pub(crate) fn new(feed: Arc<Mutex<Feed>>, from: u64) -> Self {
        Self {
            feed,
            from,
            cancelled: AtomicBool::new(false),
        }
    }

    fn render(&self, rendered: impl FnMut()) -> Option<Resync> {
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let prepared = {
            let feed = lock(&self.feed);
            if self.cancelled.load(Ordering::Relaxed) {
                return None;
            }
            feed.prepare(self.from)
        };
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        prepared.render(&self.cancelled, rendered)
    }
}

#[export]
impl Recovery {
    /// Builds the captured revision off the caller's UI thread. `None` means cancelled.
    pub fn build(&self) -> Option<Resync> {
        self.render(|| {})
    }

    /// Stops the build at its next item, before hashing, or before returning a result.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Renderer for Feed {
    fn show(&mut self, shown: Vec<Show>) -> Vec<Effect> {
        for show in shown {
            if let Some(update) = self.render(show) {
                self.producer.push(update);
            }
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
    feed: Arc<Mutex<Feed>>,
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
                machine: protocol::MachineId([0; 16]),
                upgrade: None,
            },
        });
        Self {
            feed: Arc::new(Mutex::new(Feed::new(client))),
        }
    }

    /// Folds one daemon message, as the JSON the daemon frames it in. `None`
    /// when it changed nothing.
    pub fn fold(&self, message: &str) -> Result<Option<ChangeBatch>, ClientError> {
        let msg = DaemonMsg::from_json(message.as_bytes()).map_err(|e| ClientError::Protocol {
            message: e.to_string(),
        })?;
        let mut feed = lock(&self.feed);
        let effects = feed.step(Event {
            conn: ConnId(1),
            kind: EventKind::Msg(msg),
        });
        let mut last = None;
        for effect in effects {
            // As a view alone: no link to change.
            if let Effect::Show(show @ Show::View(_)) = effect
                && let Some(update) = feed.render(show)
            {
                feed.producer.push(update.clone());
                last = Some(update);
            }
        }
        Ok(last)
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
        self.recovery(from).build().expect("uncancelled recovery")
    }

    /// Prepares a recovery handle without locking or rendering the feed.
    pub fn recovery(&self, from: u64) -> Recovery {
        Recovery::new(Arc::clone(&self.feed), from)
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
    #[test]
    fn requested_metadata_updates_native_headers_outside_the_rendered_window() {
        let reducer = Reducer::new();
        {
            let mut feed = lock(&reducer.feed);
            feed.client.release_history();
            feed.client.demand_sides([TITLE.into()]);
        }
        let entries = [
            protocol::Entry::new(protocol::Seq::new(1).unwrap(), protocol::Epoch(0), protocol::Body::ModelCall {
                model: "m".into(), purpose: Some(TITLE.into()), r#for: None, attempt: 0,
                choice: protocol::ToolChoice::None, request: None,
            }),
            protocol::Entry::new(protocol::Seq::new(2).unwrap(), protocol::Epoch(0), protocol::Body::ModelResult {
                call: protocol::CallId::new(1).unwrap(), r#for: None, failure: None,
                outcome: protocol::Outcome::Done { output: protocol::Json::encode(&serde_json::json!({"message":{"content":[{"type":"text","text":"requested label"}]}})).unwrap() },
            }),
        ];
        let mut index = protocol::view::Index::new(protocol::SessionId::new(), "/w".into());
        for entry in &entries {
            index.apply(entry);
        }
        let head = index.head_for(protocol::Epoch(0), 0, &[TITLE.into()]);
        let effects = lock(&reducer.feed).step(Event {
            conn: ConnId(1),
            kind: EventKind::Msg(DaemonMsg::Head {
                head: Box::new(head),
            }),
        });
        let (id, range) = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Io(client::Io::Send(protocol::ClientMsg::History { id, range })) => {
                    Some((id, range))
                }
                _ => None,
            })
            .expect("metadata must demand its page");
        let page = index
            .page(&range, |seq| {
                Ok(entries[usize::try_from(seq.get()).unwrap() - 1].clone())
            })
            .unwrap();
        let update = reducer
            .fold(
                &serde_json::to_string(&DaemonMsg::HistoryPart {
                    id,
                    offset: 0,
                    text: serde_json::to_string(&page).unwrap(),
                    done: true,
                })
                .unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(update.title.as_deref(), Some("requested label"));
        assert!(
            update
                .changes
                .iter()
                .any(|change| matches!(change, ViewChange::Activity))
        );
        assert_eq!(reducer.snapshot().items, [] as [crate::DisplayItem; 0]);
        assert_eq!(reducer.snapshot().title.as_deref(), Some("requested label"));
    }

    use super::*;

    #[test]
    fn recovery_releases_the_feed_during_rendering_and_cancellation_stops_remaining_items() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let reducer = Reducer::new();
        for seq in 1..=3 {
            let message = DaemonMsg::Entry(protocol::Entry::new(
                protocol::Seq::new(seq).unwrap(),
                protocol::Epoch(0),
                protocol::Body::Assistant {
                    text: "x".repeat(1 << 20),
                },
            ));
            reducer
                .fold(&serde_json::to_string(&message).unwrap())
                .unwrap();
        }
        // A lost replay range exercises the full-history rendering path.
        lock(&reducer.feed).recent.clear();
        let recovery = Arc::new(reducer.recovery(0));
        let worker = Arc::clone(&recovery);
        let (rendered, rendering) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let build = thread::spawn(move || {
            let mut count = 0;
            let result = worker.render(|| {
                count += 1;
                if count == 1 {
                    rendered.send(()).unwrap();
                    resumed.recv_timeout(Duration::from_secs(10)).unwrap();
                }
            });
            (result, count)
        });
        rendering.recv_timeout(Duration::from_secs(10)).unwrap();
        // The first item has actually rendered and two still remain. UI commands
        // must acquire this same lock before recovery finishes.
        let available = reducer.feed.try_lock().is_ok();
        recovery.cancel();
        resume.send(()).unwrap();
        let (result, count) = build.join().unwrap();
        assert!(
            available,
            "rendering must not block commands on the feed mutex"
        );
        assert!(result.is_none(), "cancelled results never reach the UI");
        assert_eq!(count, 1, "cancellation stops the remaining history render");
        let Resync::Snapshot { snapshot } = reducer.resync(0) else {
            panic!("snapshot required")
        };
        assert_eq!(
            snapshot.items.len(),
            3,
            "cancellation does not discard the view"
        );
    }

    #[test]
    fn retained_payload_releases_replaced_views_and_evicted_updates() {
        let reducer = Reducer::new();
        let message = DaemonMsg::Entry(protocol::Entry::new(
            protocol::Seq::new(1).unwrap(),
            protocol::Epoch(0),
            protocol::Body::Assistant {
                text: "x".repeat(1 << 20),
            },
        ));
        reducer
            .fold(&serde_json::to_string(&message).unwrap())
            .unwrap();
        assert!(lock(&reducer.feed).retained_bytes() >= 2 << 20);
        let empty =
            DaemonMsg::Snapshot(Box::new(protocol::view::SessionView::default().snapshot()));
        reducer
            .fold(&serde_json::to_string(&empty).unwrap())
            .unwrap();
        let mut feed = lock(&reducer.feed);
        for _ in 0..BUDGET.requests * 2 {
            feed.batch(vec![ViewChange::Error {
                message: "x".repeat(16 << 10),
            }]);
        }
        let retained = feed.retained_bytes();
        assert!(retained <= BUDGET.bytes);
        for _ in 0..BUDGET.requests * 2 {
            feed.batch(vec![ViewChange::Error {
                message: "x".repeat(16 << 10),
            }]);
        }
        assert_eq!(
            feed.retained_bytes(),
            retained,
            "lifetime traffic is not retained payload"
        );
        for _ in 0..BUDGET.requests * 2 {
            feed.batch(vec![ViewChange::Activity]);
        }
        assert!(feed.retained_bytes() < 128 << 10);
    }

    #[test]
    fn oversized_output_recovers_without_a_revision_gap_or_history_rebuild() {
        for size in [1 << 20, BUDGET.bytes * 2] {
            let reducer = Reducer::new();
            let stream = reducer.updates();
            let message = DaemonMsg::Entry(protocol::Entry::new(
                protocol::Seq::new(1).unwrap(),
                protocol::Epoch(0),
                protocol::Body::Assistant {
                    text: "x".repeat(size),
                },
            ));
            let update = reducer
                .fold(&serde_json::to_string(&message).unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(update.revision, 1);
            assert_eq!(stream.pop_event(), Some(update));
            let snapshot = reducer.snapshot();
            assert_eq!(
                snapshot.items[0]
                    .presentation
                    .body
                    .as_ref()
                    .unwrap()
                    .text
                    .len(),
                size
            );
            assert!(reducer.details(1).unwrap().contains(&"x".repeat(size)));
            let feed = lock(&reducer.feed);
            assert!(feed.bytes <= BUDGET.bytes);
            let Resync::Patch { from, update, .. } = feed.resync(0) else {
                panic!("large items recover through a patch");
            };
            assert_eq!((from, update.revision), (0, 1));
            let [ViewChange::Added { item, index: 0 }] = update.changes.as_slice() else {
                panic!("the patch adds the complete item");
            };
            assert_eq!(item.presentation.body.as_ref().unwrap().text.len(), size);
        }
    }

    #[test]
    fn stream_buffer_loss_recovers_complete_output_from_replay_or_snapshot() {
        let reducer = Reducer::new();
        let stream = reducer.updates();
        for seq in 1..=BUFFER + 4 {
            let message = DaemonMsg::Entry(protocol::Entry::new(
                protocol::Seq::new(seq as u64).unwrap(),
                protocol::Epoch(0),
                protocol::Body::Assistant {
                    text: format!("output {seq}"),
                },
            ));
            reducer
                .fold(&serde_json::to_string(&message).unwrap())
                .unwrap();
        }
        let mut revisions = Vec::new();
        while let Some(update) = stream.pop_event() {
            revisions.push(update.revision);
        }
        assert!(revisions.len() < BUFFER + 4);
        let Resync::Updates { updates } = reducer.resync(0) else {
            panic!("replay retained");
        };
        assert_eq!(updates.len(), BUFFER + 4);
        {
            let mut feed = lock(&reducer.feed);
            for _ in 0..BUDGET.requests {
                feed.batch(vec![ViewChange::Activity]);
            }
        }
        let Resync::Snapshot { snapshot } = reducer.resync(0) else {
            panic!("replay evicted");
        };
        assert_eq!(snapshot.items.len(), BUFFER + 4);
        assert_eq!(
            snapshot
                .items
                .last()
                .unwrap()
                .presentation
                .body
                .as_ref()
                .unwrap()
                .text,
            format!("output {}", BUFFER + 4)
        );
    }

    #[test]
    fn slow_readers_do_not_queue_copies_of_cumulative_output() {
        let reducer = Reducer::new();
        let stream = reducer.updates();
        let call = DaemonMsg::Entry(protocol::Entry::new(
            protocol::Seq::new(1).unwrap(),
            protocol::Epoch(0),
            protocol::Body::ToolCall {
                invoke: None,
                tool: "output".into(),
                input: protocol::Json::null(),
                id: None,
                card: None,
                by: protocol::Audience::Model,
            },
        ));
        reducer
            .fold(&serde_json::to_string(&call).unwrap())
            .unwrap();
        for seq in 2..=BUFFER + 1 {
            let progress = DaemonMsg::Entry(protocol::Entry::new(
                protocol::Seq::new(seq as u64).unwrap(),
                protocol::Epoch(0),
                protocol::Body::Progress {
                    call: protocol::CallId::new(1).unwrap(),
                    text: "x".repeat(64 << 10),
                },
            ));
            reducer
                .fold(&serde_json::to_string(&progress).unwrap())
                .unwrap();
        }
        let mut queued = 0;
        while let Some(update) = stream.pop_event() {
            queued += update.retained_bytes();
        }
        assert!(
            queued <= BUDGET.bytes,
            "queued {queued} bytes of cumulative output"
        );
        let Resync::Patch { update, .. } = reducer.resync(0) else {
            panic!("retained invalidations recover without rebuilding history");
        };
        assert_eq!(update.revision, (BUFFER + 1) as u64);
        let [ViewChange::Added { item, .. }] = update.changes.as_slice() else {
            panic!("cumulative output is rendered once");
        };
        assert_eq!(
            item.presentation.body.as_ref().unwrap().text.len(),
            BUFFER * (64 << 10)
        );

        assert_eq!(
            reducer.snapshot().items[0]
                .presentation
                .body
                .as_ref()
                .unwrap()
                .text
                .len(),
            BUFFER * (64 << 10)
        );
    }

    #[test]
    fn large_stream_recovery_keeps_complete_text_without_rebuilding_items() {
        let reducer = Reducer::new();
        let message = DaemonMsg::Entry(protocol::Entry::new(
            protocol::Seq::new(1).unwrap(),
            protocol::Epoch(0),
            protocol::Body::ModelCall {
                model: "test".into(),
                attempt: 0,
                choice: protocol::ToolChoice::default(),
                r#for: None,
                purpose: None,
                request: None,
            },
        ));
        reducer
            .fold(&serde_json::to_string(&message).unwrap())
            .unwrap();
        let chunk = "€".repeat(128 << 10);
        for _ in 0..BUFFER + 4 {
            let message = DaemonMsg::Delta {
                call: protocol::CallId::new(1).unwrap(),
                attempt: 0,
                text: chunk.clone(),
            };
            reducer
                .fold(&serde_json::to_string(&message).unwrap())
                .unwrap();
        }
        let Resync::Patch {
            update, streaming, ..
        } = reducer.resync(0)
        else {
            panic!("large stream notifications retain their recovery metadata");
        };
        assert_eq!(streaming.unwrap().text, chunk.repeat(BUFFER + 4));
        assert_eq!(
            update.retained_bytes,
            lock(&reducer.feed).retained_bytes() as u64
        );
    }

    #[test]
    fn large_item_patches_do_not_rebuild_unchanged_history() {
        let reducer = Reducer::new();
        for seq in 1..=100 {
            let message = DaemonMsg::Entry(protocol::Entry::new(
                protocol::Seq::new(seq).unwrap(),
                protocol::Epoch(0),
                protocol::Body::Assistant {
                    text: "history".into(),
                },
            ));
            reducer
                .fold(&serde_json::to_string(&message).unwrap())
                .unwrap();
        }
        let from = reducer.snapshot().rev;
        let message = DaemonMsg::Entry(protocol::Entry::new(
            protocol::Seq::new(101).unwrap(),
            protocol::Epoch(0),
            protocol::Body::Assistant {
                text: "x".repeat(1 << 20),
            },
        ));
        reducer
            .fold(&serde_json::to_string(&message).unwrap())
            .unwrap();
        let Resync::Patch { update, .. } = reducer.resync(from) else {
            panic!("one new item is a patch");
        };
        let [ViewChange::Added { index: 100, item }] = update.changes.as_slice() else {
            panic!("only the changed item crosses");
        };
        assert_eq!(item.presentation.body.as_ref().unwrap().text.len(), 1 << 20);
        assert_eq!(reducer.snapshot().items.len(), 101);
    }

    #[test]
    fn invalidated_indices_never_survive_a_view_replacement() {
        let reducer = Reducer::new();
        let message = DaemonMsg::Entry(protocol::Entry::new(
            protocol::Seq::new(1).unwrap(),
            protocol::Epoch(0),
            protocol::Body::Assistant {
                text: "x".repeat(1 << 20),
            },
        ));
        reducer
            .fold(&serde_json::to_string(&message).unwrap())
            .unwrap();
        let reset =
            DaemonMsg::Snapshot(Box::new(protocol::view::SessionView::default().snapshot()));
        reducer
            .fold(&serde_json::to_string(&reset).unwrap())
            .unwrap();
        let Resync::Snapshot { snapshot } = reducer.resync(0) else {
            panic!("a reset replaces the base");
        };
        assert_eq!(snapshot.items, Vec::<crate::DisplayItem>::new());
    }

    #[test]
    fn large_view_replacements_queue_metadata_and_recover_the_complete_view() {
        let reducer = Reducer::new();
        let message = DaemonMsg::Entry(protocol::Entry::new(
            protocol::Seq::new(1).unwrap(),
            protocol::Epoch(0),
            protocol::Body::Assistant {
                text: "x".repeat(1 << 20),
            },
        ));
        reducer
            .fold(&serde_json::to_string(&message).unwrap())
            .unwrap();
        let snapshot = DaemonMsg::Snapshot(Box::new(lock(&reducer.feed).client.view().snapshot()));
        let update = reducer
            .fold(&serde_json::to_string(&snapshot).unwrap())
            .unwrap()
            .unwrap();
        assert!(update.retained_bytes() < BUDGET.bytes / BUFFER);
        assert!(matches!(
            update.changes.as_slice(),
            [ViewChange::ViewInvalidated]
        ));
        let Resync::Snapshot { snapshot } = reducer.resync(1) else {
            panic!("a replacement needs the whole view");
        };
        assert_eq!(
            snapshot.items[0]
                .presentation
                .body
                .as_ref()
                .unwrap()
                .text
                .len(),
            1 << 20
        );
    }

    #[test]
    fn cached_charges_match_serialized_payloads_through_replacement_and_release() {
        let vector: serde_json::Value =
            serde_json::from_slice(include_bytes!("../../protocol/tests/vectors/view.json"))
                .unwrap();
        for vector in vector.as_array().unwrap() {
            let reducer = Reducer::new();
            for frame in vector["frames"].as_array().unwrap() {
                reducer.fold(frame.as_str().unwrap()).unwrap();
                let feed = lock(&reducer.feed);
                let view = feed.client.view();
                assert_eq!(
                    feed.state_bytes,
                    serde_json::to_vec(&view.state).unwrap().len()
                );
                assert_eq!(
                    feed.item_bytes,
                    view.items
                        .iter()
                        .map(|raw| {
                            serde_json::to_vec(raw).unwrap().len()
                                + crate::DisplayItem::in_view(raw, view).retained_bytes()
                        })
                        .collect::<Vec<_>>()
                );
                assert_eq!(
                    feed.bytes,
                    feed.recent
                        .iter()
                        .map(ChangeBatch::retained_bytes)
                        .sum::<usize>()
                );
                assert_eq!(feed.snapshot().retained_bytes, feed.retained_bytes() as u64);
            }
        }
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
    fn retained_state_and_rendered_text_survive_update_eviction_and_snapshots() {
        for annotation in [false, true] {
            let reducer = Reducer::new();
            fold(&reducer, 1, tool());
            let text = "€".repeat(128 << 10);
            fold(
                &reducer,
                2,
                if annotation {
                    Body::Fact {
                        fact: ExtensionFact::Annotation(Annotation {
                            target: CallId::new(1).unwrap(),
                            lifetime: AnnotationLifetime::Call,
                            card: Card {
                                body: text.clone(),
                                ..Card::default()
                            },
                        }),
                    }
                } else {
                    Body::Progress {
                        call: CallId::new(1).unwrap(),
                        text: text.clone(),
                    }
                },
            );
            {
                let mut feed = lock(&reducer.feed);
                for _ in 0..BUDGET.requests * 2 {
                    feed.batch(vec![ViewChange::Activity]);
                }
                assert!(feed.retained_bytes() >= text.len() * 2);
                assert!(feed.bytes < 128 << 10);
            }
            let snapshot =
                DaemonMsg::Snapshot(Box::new(lock(&reducer.feed).client.view().snapshot()));
            reducer
                .fold(&serde_json::to_string(&snapshot).unwrap())
                .unwrap();
            assert!(lock(&reducer.feed).retained_bytes() >= text.len() * 2);
            let empty =
                DaemonMsg::Snapshot(Box::new(protocol::view::SessionView::default().snapshot()));
            reducer
                .fold(&serde_json::to_string(&empty).unwrap())
                .unwrap();
            let mut feed = lock(&reducer.feed);
            assert_eq!(feed.payload_bytes, 0);
            for _ in 0..BUDGET.requests {
                feed.batch(vec![ViewChange::Activity]);
            }
            assert!(feed.retained_bytes() < 128 << 10);
        }
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
