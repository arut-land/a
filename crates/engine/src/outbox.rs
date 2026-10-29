//! A connection's outgoing frames. A client following a session is a
//! cursor into the session's [`FrameLog`], which every client shares: a
//! slow viewer only lags, costs the daemon no copy of what it missed, and
//! never holds up a session. What is not an entry waits in a queue, each
//! frame for the entries before it; ephemeral frames are dropped once too
//! many wait.

use std::collections::{HashSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::store::Lines;
use protocol::{BlobId, Class, Decode, Entry, Frame, Held};

use crate::lock;

use protocol::QueueBudget;

pub(crate) const BUDGET: QueueBudget = QueueBudget {
    bytes: 32 << 20,
    requests: 256,
    control_reserve: 16,
};

// Reservations cover queued session requests until their durable answer resolves.
static QUEUED: Mutex<(usize, usize)> = Mutex::new((0, 0));

// Queries count apart, for as long as each runs: however many are held
// open, none takes a mutation's place.
static ASKED: Mutex<(usize, usize)> = Mutex::new((0, 0));

// A held extension query cannot prevent a renderer from reading its session.
static HISTORY: Mutex<(usize, usize)> = Mutex::new((0, 0));

#[derive(Clone, Copy)]
pub(super) enum ReadClass {
    Query,
    History,
}

pub(super) struct QueuePermit(&'static Mutex<(usize, usize)>, usize);

impl QueuePermit {
    pub(super) fn acquire(bytes: usize, control: bool) -> Option<Self> {
        Self::from(&QUEUED, bytes, control)
    }

    /// A running query's.
    pub(super) fn query(bytes: usize, class: ReadClass) -> Option<Self> {
        Self::from(
            match class {
                ReadClass::Query => &ASKED,
                ReadClass::History => &HISTORY,
            },
            bytes,
            false,
        )
    }

    fn from(pool: &'static Mutex<(usize, usize)>, bytes: usize, control: bool) -> Option<Self> {
        let mut queued = lock(pool);
        if !BUDGET.admits(queued.1.saturating_add(bytes), queued.0, control) {
            return None;
        }
        queued.0 += 1;
        queued.1 += bytes;
        Some(Self(pool, bytes))
    }
}

impl Drop for QueuePermit {
    fn drop(&mut self) {
        let mut queued = lock(self.0);
        queued.0 -= 1;
        queued.1 -= self.1;
    }
}

/// A session's committed log and shared public-history seek index.
pub struct FrameLog {
    pub(crate) history: crate::history::History,
    lines: Mutex<Arc<dyn Lines>>,
    /// The seq of the last entry shown. Entries are shown under the lock
    /// they commit under, after the log holds them.
    seq: AtomicU64,
    /// The blobs the entries reference, folded only as far as a client
    /// last asked about one: a session nobody fetches from pays nothing.
    held: Mutex<Referenced>,
}

#[derive(Default)]
struct Referenced {
    /// The seq of the last entry folded.
    through: u64,
    blobs: HashSet<BlobId>,
}

impl FrameLog {
    /// The log of `lines`, whose entries up to `seq` are committed.
    pub fn with_owner(lines: Arc<dyn Lines>, seq: u64, owner: crate::history::Owner) -> Self {
        Self {
            history: crate::history::History::new(owner),
            lines: Mutex::new(lines),
            seq: AtomicU64::new(seq),
            held: Mutex::default(),
        }
    }

    #[cfg(test)]
    pub fn new(lines: Arc<dyn Lines>, seq: u64) -> Self {
        Self::with_owner(
            lines,
            seq,
            crate::history::Owner {
                session: protocol::SessionId::new(),
                workspace: "/test".into(),
                generation: protocol::Epoch(1),
            },
        )
    }

    /// Whether an entry committed so far references blob `blob`: what a
    /// client may fetch through the session. Blocks: reads the entries
    /// not folded yet.
    pub fn references(&self, blob: &BlobId) -> io::Result<bool> {
        let mut folded = lock(&self.held);
        if folded.blobs.contains(blob) {
            return Ok(true);
        }
        for batch in self.batches_after(folded.through) {
            for entry in batch? {
                folded.through = entry.seq.get();
                folded.blobs.extend(
                    entry
                        .body
                        .blobs()
                        .into_iter()
                        .filter_map(|held| match held {
                            Held::Blob(blob) if blob.origin != protocol::BlobOrigin::Context => {
                                Some(blob.id)
                            }
                            Held::Blob(_) | Held::Stream(_) => None,
                        }),
                );
            }
        }
        Ok(folded.blobs.contains(blob))
    }

    /// A reload replaces the reader after taking a new writer lease.
    pub fn reloaded(&self, lines: Arc<dyn Lines>) {
        *lock(&self.lines) = lines;
    }

    /// The seq of the last entry committed.
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Publishes the next committed sequence without retaining its payload.
    pub fn push(&self, seq: u64) {
        let last = self.seq.swap(seq, Ordering::Release);
        debug_assert_eq!(last + 1, seq, "entries are shown in order");
    }

    /// The entries after `seq`, decoded from their lines a run at a time,
    /// in order: for what reads the log rarely, such as recovery after a
    /// failure.
    pub fn batches_after(&self, seq: u64) -> impl Iterator<Item = io::Result<Vec<Entry>>> + '_ {
        let last = self.seq();
        let mut cursor = seq;
        std::iter::from_fn(move || {
            if cursor >= last {
                return None;
            }
            Some(
                match self.batch(cursor + 1, last).and_then(|run| {
                    lines(&run)
                        .map(|line| Entry::from_json(line).map_err(io::Error::other))
                        .collect::<io::Result<Vec<_>>>()
                }) {
                    Ok(entries) => {
                        cursor += entries.len() as u64;
                        Ok(entries)
                    }
                    Err(error) => {
                        cursor = last;
                        Err(error)
                    }
                },
            )
        })
    }

    pub fn entries_after(&self, seq: u64) -> io::Result<Vec<Entry>> {
        self.batches_after(seq)
            .try_fold(Vec::new(), |mut entries, batch| {
                entries.extend(batch?);
                Ok(entries)
            })
    }

    pub fn entries(&self) -> io::Result<Vec<Entry>> {
        self.entries_after(0)
    }

    fn batch(&self, next: u64, last: u64) -> io::Result<Vec<u8>> {
        #[derive(serde::Deserialize)]
        struct Position {
            seq: protocol::Seq,
        }
        let mut run = Vec::new();
        let reader = Arc::clone(&lock(&self.lines));
        let count = reader.read(next..=last, RUN, &mut run)?;
        if count == 0 || count > last - next + 1 || run.last() != Some(&b'\n') {
            return Err(io::Error::other("log read made no valid progress"));
        }
        // The store validates entries when loading or appending. Recheck
        // the read's shape and sequence without allocating their bodies.
        let mut read = 0;
        for line in lines(&run) {
            let position: Position = serde_json::from_slice(line).map_err(io::Error::other)?;
            if position.seq.get() != next + read {
                return Err(io::Error::other("log batch is not contiguous"));
            }
            read += 1;
        }
        if read != count {
            return Err(io::Error::other("log batch is not contiguous"));
        }
        Ok(run)
    }

    pub(crate) fn entry(&self, seq: u64) -> io::Result<Entry> {
        let run = self.batch(seq, seq)?;
        Entry::from_json(
            lines(&run)
                .next()
                .ok_or_else(|| io::Error::other("missing history fact"))?,
        )
        .map_err(io::Error::other)
    }

    fn read_after(&self, after: u64, ahead: &mut VecDeque<io::Result<Frame>>) {
        if after >= self.seq() {
            return;
        }
        match self
            .history
            .head(self, after)
            .and_then(|head| head.frames())
        {
            Ok(frames) => ahead.extend(frames.into_iter().map(Ok)),
            Err(error) => ahead.push_back(Err(error)),
        }
    }
}

/// How many bytes of lines past the first one read of the log takes: the
/// most a client that lags holds of what it has yet to be sent.
const RUN: usize = 16 << 10;

/// The lines of a run read, each of which ends in a newline, without it.
fn lines(run: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut start = 0;
    memchr::memchr_iter(b'\n', run).map(move |end| {
        let line = &run[start..end];
        start = end + 1;
        line
    })
}

/// The sending side. The connection's own is for its replies; the one
/// [`Outbox::follow`] returns is a session's, which stops taking frames
/// once the connection follows another session or none.
#[derive(Clone)]
pub struct Outbox {
    shared: Arc<Shared>,
    /// The number of the session it follows; 0 for the connection's own.
    following: u64,
}

/// Registrations are owned by their outbox, never by a publisher. Removing
/// one does not change the outbox's queued frames or session-switch fences.
#[derive(Default)]
pub(crate) struct Subscribers(Arc<Mutex<Vec<Subscriber>>>);

#[derive(Clone)]
struct Subscriber {
    shared: Weak<Shared>,
    following: u64,
}

struct Registration {
    subscribers: Weak<Mutex<Vec<Subscriber>>>,
    subscriber: Subscriber,
}

#[derive(Default)]
struct Registrations {
    watching: Option<Registration>,
    following: Option<Registration>,
}

impl Subscribers {
    pub(crate) fn is_empty(&self) -> bool {
        lock(&self.0).is_empty()
    }

    pub(crate) fn push(&self, outbox: &Outbox) {
        let mut registrations = lock(&outbox.shared.registrations);
        let registration = if outbox.following == 0 {
            &mut registrations.watching
        } else {
            &mut registrations.following
        };
        {
            let q = outbox.queue();
            if q.closed || (outbox.following != 0 && outbox.following != q.follows) {
                return;
            }
        }
        // Repeating WatchSessions replaces the list, not its registration.
        drop(registration.take());
        let subscriber = Subscriber {
            shared: Arc::downgrade(&outbox.shared),
            following: outbox.following,
        };
        lock(&self.0).push(subscriber.clone());
        *registration = Some(Registration {
            subscribers: Arc::downgrade(&self.0),
            subscriber,
        });
    }

    pub(crate) fn retain(&self, mut send: impl FnMut(&Outbox) -> bool) {
        // Drop upgraded owners outside the registry lock: the last owner
        // unregisters itself, including when a connection ends mid-publish.
        let outboxes: Vec<_> = lock(&self.0)
            .iter()
            .filter_map(|subscriber| {
                Some(Outbox {
                    shared: subscriber.shared.upgrade()?,
                    following: subscriber.following,
                })
            })
            .collect();
        for outbox in outboxes {
            if !send(&outbox) {
                Subscriber {
                    shared: Arc::downgrade(&outbox.shared),
                    following: outbox.following,
                }
                .unregister(&self.0);
            }
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let Some(subscribers) = self.subscribers.upgrade() else {
            return;
        };
        self.subscriber.unregister(&subscribers);
    }
}

impl Subscriber {
    fn unregister(&self, subscribers: &Mutex<Vec<Self>>) {
        let mut subscribers = lock(subscribers);
        subscribers.retain(|subscriber| {
            subscriber.following != self.following || !subscriber.shared.ptr_eq(&self.shared)
        });
        if subscribers.is_empty() {
            *subscribers = Vec::new();
        }
    }
}

struct Shared {
    registrations: Mutex<Registrations>,
    queue: Mutex<Queue>,
    /// Notified as there may be more to write: the writer's one wake.
    ready: rt::Notify,
    /// Notified as the writer takes frames, or the connection ends: what
    /// waits to send paced waits here.
    room: rt::Notify,
}

#[derive(Default)]
struct Queue {
    /// Frames to write once the client has every entry up to the seq each
    /// waits for, in order.
    frames: VecDeque<Queued>,
    bytes: usize,
    follow: Option<Follow>,
    /// The newest follow, whose entries newly queued frames depend on.
    target: Option<Arc<FrameLog>>,
    /// The number of the last session followed.
    follows: u64,
    /// The writer is gone.
    closed: bool,
}

enum Queued {
    Frame(u64, Frame),
    Switch(Option<Follow>),
}

/// The session a client follows, and how far it has read its entries.
struct Follow {
    generation: u64,
    log: Arc<FrameLog>,
    /// The seq of the last entry written.
    cursor: u64,
    /// The frames of the entries after it, read from the log and not yet
    /// written: at most one read's worth.
    ahead: VecDeque<io::Result<Frame>>,
}

/// The connection is gone, or follows another session.
#[derive(Debug)]
pub struct Closed;

impl Outbox {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                registrations: Mutex::default(),
                queue: Mutex::default(),
                ready: rt::Notify::new(),
                room: rt::Notify::new(),
            }),
            following: 0,
        }
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        lock(&self.shared.queue)
    }

    /// Queues `frame` behind every entry committed so far.
    pub fn send(&self, frame: Frame) -> Result<(), Closed> {
        let mut q = self.queue();
        let after = q.target.as_ref().map_or(0, |log| log.seq());
        self.push(&mut q, after, frame)
    }

    /// [`Outbox::send`], once what waits to be written fits in half the
    /// budget with `frame`, or nothing waits: a producer that can wait (a
    /// query's streamed answer) never overruns a slow client, and leaves
    /// room for the control frames that cannot wait.
    pub async fn send_paced(&self, frame: Frame) -> Result<(), Closed> {
        self.send_within(frame, BUDGET.bytes / 2).await
    }

    /// [`Outbox::send_paced`], once what waits fits in `window` bytes with
    /// `frame`: a producer of bulk (a blob's chunks) keeps no more of
    /// itself queued than that.
    pub async fn send_within(&self, frame: Frame, window: usize) -> Result<(), Closed> {
        let len = frame.as_bytes().len();
        loop {
            let mut seen = self.shared.room.count();
            {
                // Checked and pushed under one lock, so senders that wait
                // together never overshoot together.
                let mut q = self.queue();
                if q.closed
                    || q.frames.is_empty()
                    || (q.frames.len() < BUDGET.requests / 2
                        && q.bytes.saturating_add(len) <= window)
                {
                    let after = q.target.as_ref().map_or(0, |log| log.seq());
                    return self.push(&mut q, after, frame);
                }
            }
            self.shared.room.changed(&mut seen).await;
        }
    }

    /// Queues `frame` behind entry `after`. Callers pass the seq of the
    /// last entry committed, holding the lock under which entries commit.
    pub fn send_after(&self, after: u64, frame: Frame) -> Result<(), Closed> {
        self.push(&mut self.queue(), after, frame)
    }

    fn push(&self, q: &mut Queue, after: u64, frame: Frame) -> Result<(), Closed> {
        if q.closed || (self.following != 0 && self.following != q.follows) {
            return Err(Closed);
        }
        if let Some(kind) = frame.replaceable {
            // A replacement cannot cross a session switch's ordering fence.
            let start = q
                .frames
                .iter()
                .rposition(|item| matches!(item, Queued::Switch(_)))
                .map_or(0, |i| i + 1);
            let mut index = 0;
            q.frames.retain(|item| {
                let replace = index >= start
                    && matches!(item, Queued::Frame(_, queued) if queued.replaceable == Some(kind));
                index += 1;
                if replace && let Queued::Frame(_, queued) = item {
                    q.bytes -= queued.as_bytes().len();
                }
                !replace
            });
        }
        let len = frame.as_bytes().len();
        if frame.class == Class::Ephemeral
            && (q.frames.len() >= BUDGET.requests - BUDGET.control_reserve
                || q.bytes.saturating_add(len) > BUDGET.bytes / 2)
        {
            return Ok(());
        }
        let reserved = frame.replaceable.is_none() || {
            let (bytes, requests) = q
                .frames
                .iter()
                .filter_map(|item| match item {
                    Queued::Frame(_, frame) => Some(frame),
                    Queued::Switch(_) => None,
                })
                .filter(|frame| frame.class == Class::Ephemeral || frame.replaceable.is_some())
                .fold((len, 0), |(bytes, requests), frame| {
                    (bytes.saturating_add(frame.as_bytes().len()), requests + 1)
                });
            BUDGET.admits(bytes, requests, false)
        };
        if !reserved || !BUDGET.admits(q.bytes.saturating_add(len), q.frames.len(), true) {
            q.closed = true;
            q.follow = None;
            q.target = None;
            q.frames.clear();
            q.bytes = 0;
            self.shared.ready.notify();
            self.shared.room.notify();
            return Err(Closed);
        }
        q.bytes += len;
        q.frames.push_back(Queued::Frame(after, frame));
        self.shared.ready.notify();
        Ok(())
    }

    /// Wakes the writer: entries committed.
    pub fn wake(&self) -> Result<(), Closed> {
        let q = self.queue();
        if q.closed || (self.following != 0 && self.following != q.follows) {
            return Err(Closed);
        }
        self.shared.ready.notify();
        Ok(())
    }

    /// Follows the session whose entries `log` holds, in place of any
    /// followed before, from entry `cursor` on; then `frames`, each after
    /// the entry it names. Callers hold the lock under which the session's
    /// entries commit. The outbox returned is the session's.
    #[must_use]
    pub(crate) fn follow_head(
        &self,
        log: Arc<FrameLog>,
        head: Vec<Frame>,
        frames: Vec<(u64, Frame)>,
    ) -> Self {
        self.follow_from(log, 0, head.into_iter().map(Ok).collect(), frames)
    }

    #[cfg(test)]
    pub fn follow(&self, log: Arc<FrameLog>, cursor: u64, frames: Vec<(u64, Frame)>) -> Self {
        self.follow_from(log, cursor, VecDeque::new(), frames)
    }

    fn follow_from(
        &self,
        log: Arc<FrameLog>,
        cursor: u64,
        ahead: VecDeque<io::Result<Frame>>,
        frames: Vec<(u64, Frame)>,
    ) -> Self {
        let mut registrations = lock(&self.shared.registrations);
        drop(registrations.following.take());
        let mut q = self.queue();
        q.switch(Some((log, cursor, ahead)));
        for (after, frame) in frames {
            if self.push(&mut q, after, frame).is_err() {
                break;
            }
        }
        self.shared.ready.notify();
        self.shared.room.notify();
        Self {
            shared: Arc::clone(&self.shared),
            following: q.follows,
        }
    }

    /// Stops following a session.
    pub fn unfollow(&self) {
        let mut registrations = lock(&self.shared.registrations);
        drop(registrations.following.take());
        self.queue().switch(None);
        self.shared.ready.notify();
        self.shared.room.notify();
    }

    /// The next frame to write, once there is one; `None` once closed and
    /// all written. The writer waits here, a task: no thread per client.
    pub async fn next(&self, rt: &dyn rt::Runtime) -> Option<Frame> {
        loop {
            let mut seen = self.shared.ready.count();
            if let Some(frame) = self.try_ready() {
                return Some(frame);
            }
            let step = self.queue().step();
            match step {
                Step::Read(log, cursor, follows) if cursor < log.seq() => {
                    let ahead = rt::try_unblock_with(
                        rt,
                        rt::BlockingClass::Storage,
                        rt::STORAGE_BYTES,
                        move || {
                            let mut ahead = VecDeque::new();
                            log.read_after(cursor, &mut ahead);
                            ahead
                        },
                    )
                    .await
                    .unwrap_or_else(|error| VecDeque::from([Err(error)]));
                    self.accept_read(cursor, follows, ahead);
                    continue;
                }
                Step::Frame(frame) => {
                    self.shared.room.notify();
                    return Some(frame);
                }
                Step::Nothing | Step::Read(..) => {}
            }
            if self.queue().closed {
                return None;
            }
            self.shared.ready.changed(&mut seen).await;
        }
    }

    /// Draining a write batch must not perform cold I/O on an async worker;
    /// only frames already in memory are available here.
    pub fn try_ready(&self) -> Option<Frame> {
        match self.queue().step() {
            Step::Frame(frame) => {
                self.shared.room.notify();
                Some(frame)
            }
            Step::Nothing | Step::Read(..) => None,
        }
    }

    #[cfg(test)]
    pub fn try_next(&self) -> Option<Frame> {
        loop {
            let step = self.queue().step();
            match step {
                Step::Frame(frame) => {
                    self.shared.room.notify();
                    return Some(frame);
                }
                Step::Read(log, cursor, follows) if cursor < log.seq() => {
                    let mut ahead = VecDeque::new();
                    log.read_after(cursor, &mut ahead);
                    self.accept_read(cursor, follows, ahead);
                }
                _ => return None,
            }
        }
    }

    fn accept_read(&self, cursor: u64, follows: u64, ahead: VecDeque<io::Result<Frame>>) {
        let mut q = self.queue();
        if let Some(follow) = &mut q.follow
            && follow.generation == follows
            && follow.cursor == cursor
        {
            follow.ahead = ahead;
        }
    }

    /// Ends the connection once what is queued is written: sends fail from
    /// now on.
    pub fn close(&self) {
        self.queue().closed = true;
        drop(std::mem::take(&mut *lock(&self.shared.registrations)));
        self.shared.ready.notify();
        self.shared.room.notify();
    }

    /// Ends the connection now, dropping what is queued: the writer failed.
    pub fn abandon(&self) {
        let mut q = self.queue();
        q.closed = true;
        q.follow = None;
        q.target = None;
        q.frames.clear();
        q.bytes = 0;
        drop(q);
        drop(std::mem::take(&mut *lock(&self.shared.registrations)));
        self.shared.ready.notify();
        self.shared.room.notify();
    }
}

/// What the queue has next.
enum Step {
    Frame(Frame),
    /// Nothing now.
    Nothing,
    /// The next entries of the session followed, read from its log after
    /// this cursor; `.2` is the follow they are for.
    Read(Arc<FrameLog>, u64, u64),
}

impl Queue {
    /// The next frame to write, if there is one now: the next queued if the
    /// client has the entries it waits for, else the next entry.
    fn step(&mut self) -> Step {
        while let Some(queued) = self.frames.pop_front_if(|queued| match queued {
            Queued::Switch(_) => true,
            Queued::Frame(after, _) => self.follow.as_ref().is_none_or(|f| *after <= f.cursor),
        }) {
            match queued {
                Queued::Switch(follow) => self.follow = follow,
                Queued::Frame(_, frame) => {
                    self.bytes -= frame.as_bytes().len();
                    return Step::Frame(frame);
                }
            }
        }
        let Some(follow) = self.follow.as_mut() else {
            return Step::Nothing;
        };
        match follow.ahead.pop_front() {
            None => Step::Read(Arc::clone(&follow.log), follow.cursor, follow.generation),
            Some(Ok(frame)) => {
                follow.cursor = frame.seq.unwrap_or(follow.cursor);
                Step::Frame(frame)
            }
            // Queued snapshots and acknowledgements certify a prefix we cannot deliver.
            Some(Err(_)) => {
                self.closed = true;
                self.follow = None;
                self.target = None;
                self.frames.clear();
                self.bytes = 0;
                Step::Nothing
            }
        }
    }

    /// Retires the producer now; the writer changes logs only after every
    /// queued frame and its entry dependencies have drained.
    fn switch(&mut self, next: Option<(Arc<FrameLog>, u64, VecDeque<io::Result<Frame>>)>) {
        self.follows += 1;
        if self.closed {
            return;
        }
        if !BUDGET.admits(self.bytes, self.frames.len(), true) {
            self.closed = true;
            self.follow = None;
            self.target = None;
            self.frames.clear();
            self.bytes = 0;
            return;
        }
        self.target = next.as_ref().map(|(log, _, _)| Arc::clone(log));
        let follow = next.map(|(log, cursor, ahead)| Follow {
            generation: self.follows,
            log,
            cursor,
            ahead,
        });
        if self.frames.is_empty() {
            self.follow = follow;
        } else if let Some(Queued::Switch(queued)) = self.frames.back_mut() {
            *queued = follow;
        } else {
            self.frames.push_back(Queued::Switch(follow));
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::{Body, CallId, DaemonMsg, Epoch, Seq};

    use super::*;

    fn entry(seq: u64, text: &str) -> Frame {
        Frame::entry(&Entry::new(
            Seq::new(seq).unwrap(),
            Epoch(0),
            Body::Input {
                segments: text.into(),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: None,
                force: false,
            },
        ))
        .unwrap()
    }

    #[test]
    fn disconnected_subscribers_unregister_without_publication() {
        for end in [Outbox::close, Outbox::abandon] {
            let subscribers = Subscribers::default();
            let outbox = Outbox::new();
            subscribers.push(&outbox);
            assert_eq!(lock(&subscribers.0).len(), 1);
            end(&outbox);
            assert!(subscribers.is_empty());
            assert_eq!(lock(&subscribers.0).capacity(), 0);
        }
        let subscribers = Subscribers::default();
        let outbox = Outbox::new();
        subscribers.push(&outbox);
        drop(outbox);
        assert!(subscribers.is_empty());
        assert_eq!(lock(&subscribers.0).capacity(), 0);
    }

    #[test]
    fn the_last_subscriber_owner_can_end_during_publication() {
        let subscribers = Subscribers::default();
        let mut outbox = Some(Outbox::new());
        subscribers.push(outbox.as_ref().unwrap());
        subscribers.retain(|_| {
            drop(outbox.take());
            true
        });
        assert!(subscribers.is_empty());
        assert_eq!(lock(&subscribers.0).capacity(), 0);
    }

    #[test]
    fn unfollow_unregisters_only_the_session_and_preserves_queued_frames() {
        let watchers = Subscribers::default();
        let followers = Subscribers::default();
        let outbox = Outbox::new();
        watchers.push(&outbox);
        let shown = log(&["before detach"]);
        followers.push(&outbox.follow(
            shown.frames(),
            0,
            vec![(1, Frame::new(&DaemonMsg::Synced { seq: 1 }).unwrap())],
        ));
        outbox.unfollow();
        assert!(followers.is_empty());
        assert_eq!(lock(&followers.0).capacity(), 0);
        assert!(!watchers.is_empty());
        outbox.close();
        assert!(watchers.is_empty());
        assert_eq!(written(&outbox), vec![Some(1), None]);
    }

    #[test]
    fn repeated_watches_and_session_switches_replace_their_registration() {
        let watchers = Subscribers::default();
        let old = Subscribers::default();
        let new = Subscribers::default();
        let outbox = Outbox::new();
        watchers.push(&outbox);
        watchers.push(&outbox);
        assert_eq!(lock(&watchers.0).len(), 1);
        let shown = log(&[]);
        old.push(&outbox.follow(shown.frames(), 0, Vec::new()));
        new.push(&outbox.follow(shown.frames(), 0, Vec::new()));
        assert!(old.is_empty());
        assert_eq!(lock(&new.0).len(), 1);
        assert_eq!(lock(&watchers.0).len(), 1);
        outbox.close();
        assert!(new.is_empty());
        assert!(watchers.is_empty());
    }

    #[test]
    fn replaceable_session_lists_preserve_control_capacity() {
        let outbox = Outbox::new();
        let mut summary = protocol::SessionSummary {
            id: protocol::SessionId::new(),
            workspace: ".".into(),
            entries: 0,
            clients: 1,
            activity: protocol::Activity::Idle,
            residency: protocol::Residency::Loaded,
            read_only: None,
        };
        for entries in 0..BUDGET.requests as u64 {
            summary.entries = entries;
            outbox
                .send(
                    Frame::new(&DaemonMsg::Sessions {
                        sessions: vec![summary.clone()],
                    })
                    .unwrap(),
                )
                .unwrap();
        }
        let latest = Frame::new(&DaemonMsg::Sessions {
            sessions: Vec::new(),
        })
        .unwrap();
        outbox.send(latest.clone()).unwrap();
        let q = outbox.queue();
        assert_eq!(q.frames.len(), 1);
        assert_eq!(
            match q.frames.back().unwrap() {
                Queued::Frame(_, frame) => frame.as_bytes(),
                Queued::Switch(_) => panic!("expected frame"),
            },
            latest.as_bytes()
        );
    }

    #[test]
    fn replaceable_status_and_settings_keep_the_latest_prefix_under_saturation() {
        let outbox = Outbox::new();
        let mut latest = Vec::new();
        for prefix in 0..=BUDGET.requests as u64 {
            latest = [
                DaemonMsg::Status {
                    pid: 1,
                    build: String::new(),
                    started: 0,
                    clients: 1,
                    known: u32::try_from(prefix).unwrap(),
                },
                DaemonMsg::Settings {
                    settings: protocol::HomeSettings::default(),
                    error: Some(prefix.to_string()),
                },
            ]
            .iter()
            .map(|msg| Frame::new(msg).unwrap())
            .collect();
            for frame in &latest {
                outbox.send_after(prefix, frame.clone()).unwrap();
            }
        }
        assert_eq!(outbox.queue().frames.len(), 2);
        for _ in 2..BUDGET.requests - BUDGET.control_reserve {
            outbox.send(delta()).unwrap();
        }
        for client_seq in 0..BUDGET.control_reserve as u64 {
            outbox
                .send(Frame::new(&DaemonMsg::Ack { client_seq }).unwrap())
                .unwrap();
        }
        // Replacing at full capacity must preserve the newest ordering fence.
        for frame in &latest {
            outbox.send_after(999, frame.clone()).unwrap();
        }
        let q = outbox.queue();
        assert!(!q.closed);
        assert_eq!(q.frames.len(), BUDGET.requests);
        assert_eq!(
            q.bytes,
            q.frames
                .iter()
                .filter_map(|item| match item {
                    Queued::Frame(_, frame) => Some(frame.as_bytes().len()),
                    Queued::Switch(_) => None,
                })
                .sum::<usize>()
        );
        for (item, expected) in q.frames.iter().rev().take(2).zip(latest.iter().rev()) {
            let Queued::Frame(after, frame) = item else {
                panic!("expected frame")
            };
            assert_eq!(*after, 999);
            assert_eq!(frame.as_bytes(), expected.as_bytes());
        }
    }

    #[test]
    fn slow_consumers_have_bounded_retained_bytes() {
        let outbox = Outbox::new();
        let frame = Frame::new(&DaemonMsg::Error {
            message: "x".repeat(1 << 20),
        })
        .unwrap();
        for _ in 0..BUDGET.requests {
            if outbox.send(frame.clone()).is_err() {
                break;
            }
            let q = outbox.queue();
            assert!(q.bytes <= BUDGET.bytes);
            assert!(q.frames.len() <= BUDGET.requests);
        }
        assert!(outbox.queue().closed);
        assert_eq!(outbox.queue().bytes, 0);
        assert!(outbox.try_next().is_none());
    }

    #[test]
    fn durable_entries_are_never_silently_dropped() {
        let log = log(&["one", "two", "three"]);
        let outbox = Outbox::new();
        let _session = outbox.follow(log.frames(), 0, Vec::new());
        let frame = Frame::new(&DaemonMsg::Ack { client_seq: 1 }).unwrap();
        for _ in 0..BUDGET.requests {
            outbox.send(frame.clone()).unwrap();
        }
        assert!(outbox.send(frame).is_err());
        assert!(outbox.queue().closed);
        let reconnected = Outbox::new();
        let _session = reconnected.follow(log.frames(), 0, Vec::new());
        assert_eq!(written(&reconnected), [Some(3)]);
    }

    #[test]
    fn a_paced_sender_waits_for_the_writer_and_never_closes_the_connection() {
        use std::future::Future;
        let outbox = Outbox::new();
        let frame = Frame::new(&DaemonMsg::Ack { client_seq: 1 }).unwrap();
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut written = 0;
        for _ in 0..BUDGET.requests * 4 {
            let mut send = std::pin::pin!(outbox.send_paced(frame.clone()));
            while send.as_mut().poll(&mut cx).is_pending() {
                assert!(outbox.try_next().is_some());
                written += 1;
            }
            let q = outbox.queue();
            assert!(!q.closed);
            assert!(q.frames.len() <= BUDGET.requests / 2);
        }
        assert!(written > 0, "the sender never waited");
    }

    #[test]
    fn a_windowed_sender_never_queues_more_than_its_window() {
        use std::future::Future;
        let outbox = Outbox::new();
        let bytes = [7; 1000];
        let chunk = protocol::Chunk {
            blob: BlobId([1; 32]),
            offset: 0,
            bytes: &bytes,
        };
        let frame = Frame::chunk(chunk).unwrap();
        let window = 4 * frame.as_bytes().len();
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut written = 0;
        for _ in 0..64 {
            let mut send = std::pin::pin!(outbox.send_within(frame.clone(), window));
            while send.as_mut().poll(&mut cx).is_pending() {
                assert!(outbox.try_next().is_some());
                written += 1;
            }
            assert!(outbox.queue().bytes <= window);
        }
        assert!(written >= 64 - 4, "the sender queued past its window");
    }

    fn delta() -> Frame {
        Frame::new(&DaemonMsg::Delta {
            call: CallId::new(1).unwrap(),
            text: "t".into(),
            attempt: 0,
        })
        .unwrap()
    }

    /// A session's log, and the frame log that shows it.
    struct Shown(crate::store::Loaded, Arc<FrameLog>);

    impl Shown {
        fn push(&mut self, text: &str) {
            let e = self
                .0
                .append(Body::Input {
                    segments: text.into(),
                    mode: protocol::Mode::Queue,
                    taken: true,
                    origin: None,
                    force: false,
                })
                .unwrap();
            self.1.push(e.seq.get());
        }

        fn frames(&self) -> Arc<FrameLog> {
            Arc::clone(&self.1)
        }
    }

    fn log(texts: &[&str]) -> Shown {
        let header = crate::store::Header {
            id: protocol::SessionId::new(),
            workspace: "/w".into(),
            parent: None,
        };
        let loaded =
            crate::store::LogStore::create(&crate::store::Memory::default(), &header).unwrap();
        let frames = Arc::new(FrameLog::new(Arc::clone(&loaded.lines), 0));
        let mut log = Shown(loaded, frames);
        for text in texts {
            log.push(text);
        }
        log
    }

    /// The seqs of the frames written, until none is ready.
    fn written(outbox: &Outbox) -> Vec<Option<u64>> {
        std::iter::from_fn(|| outbox.try_next())
            .map(|f| f.seq)
            .collect()
    }

    /// Lines that count how often they are read.
    #[test]
    fn log_read_failure_never_becomes_a_valid_prefix() {
        struct Broken {
            lines: Arc<dyn Lines>,
            fault: u8,
        }
        impl Lines for Broken {
            fn read(
                &self,
                seqs: std::ops::RangeInclusive<u64>,
                _: usize,
                out: &mut Vec<u8>,
            ) -> io::Result<u64> {
                if *seqs.start() == 1 {
                    return self.lines.read(1..=1, RUN, out);
                }
                match self.fault {
                    0 => Err(io::Error::other("read failed after a valid entry")),
                    1 => Ok(0),
                    2 => self.lines.read(3..=3, RUN, out),
                    3 => {
                        self.lines.read(2..=2, RUN, out)?;
                        out.truncate(out.len() / 2);
                        out.push(b'\n');
                        Ok(1)
                    }
                    _ => {
                        self.lines.read(2..=2, RUN, out)?;
                        Ok(2)
                    }
                }
            }
        }
        let shown = log(&["first", "second", "third"]);
        for fault in 0..5 {
            let frames = Arc::new(FrameLog::new(
                Arc::new(Broken {
                    lines: Arc::clone(&shown.0.lines),
                    fault,
                }),
                3,
            ));
            assert!(
                frames.entries().is_err(),
                "fault {fault} cannot certify a prefix"
            );
            let transcript = crate::transcript::Transcript::replay(
                |each| {
                    for batch in frames.batches_after(0) {
                        for entry in batch? {
                            each(&entry);
                        }
                    }
                    Ok(())
                },
                Arc::new(ext::host::Registry::default()),
                "",
                &ext::blob::Places::default(),
            );
            assert!(
                transcript.is_err(),
                "fault {fault} cannot certify a transcript"
            );
            let outbox = Outbox::new();
            let follower = outbox.follow(
                frames,
                0,
                vec![(3, Frame::new(&DaemonMsg::Synced { seq: 3 }).unwrap())],
            );
            assert_eq!(written(&outbox), []);
            assert!(follower.wake().is_err());
        }
    }

    #[test]
    fn queued_inputs_larger_than_a_frame_remain_complete_across_a_control_image() {
        let mut shown = log(&[]);
        let text = "x".repeat(1 << 20);
        for _ in 0..17 {
            let entry = shown
                .0
                .append(Body::Input {
                    segments: text.clone().into(),
                    mode: protocol::Mode::Queue,
                    taken: false,
                    origin: None,
                    force: false,
                })
                .unwrap();
            shown.1.push(entry.seq.get());
        }
        let head = shown.1.history.head(&shown.1, 0).unwrap();
        let outbox = Outbox::new();
        let _follower = outbox.follow_head(
            shown.frames(),
            head.frames().unwrap(),
            vec![(17, Frame::new(&DaemonMsg::Synced { seq: 17 }).unwrap())],
        );
        assert!(outbox.queue().bytes < 1024);
        let mut control = protocol::history::Heads::default();
        let mut got = None;
        while let Some(frame) = outbox.try_next() {
            match protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes())
                .unwrap()
                .unwrap()
            {
                DaemonMsg::HeadPart {
                    seq,
                    offset,
                    text,
                    done,
                } => {
                    if let Some(head) = control.part(seq, offset, &text, done).unwrap() {
                        got = Some(head);
                    }
                }
                DaemonMsg::Synced { seq } => assert_eq!(got.as_ref().unwrap().cut.seq, seq),
                msg => panic!("unexpected public control message {msg:?}"),
            }
        }
        let got = got.unwrap();
        assert_eq!(got.state.queued().count(), 17);
        assert!(got.state.queued().all(|input| input.text == text));
        assert_eq!(got.count, 0);
    }

    #[test]
    fn public_heads_coalesce_a_slow_followers_history_without_carrying_its_text() {
        let mut log = log(&[]);
        let outbox = Outbox::new();
        let follower = outbox.follow(log.frames(), 0, Vec::new());
        for _ in 0..64 {
            log.push(&"x".repeat(128 << 10));
            follower.wake().unwrap();
        }
        let frame = outbox.try_next().unwrap();
        assert_eq!(frame.seq, Some(64));
        assert!(frame.as_bytes().len() < 4096);
        assert!(matches!(
            protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes())
                .unwrap()
                .unwrap(),
            DaemonMsg::Head { .. }
        ));
        assert!(outbox.try_next().is_none());
    }

    #[test]
    fn deltas_past_the_budget_are_dropped() {
        let outbox = Outbox::new();
        let big = Frame::new(&DaemonMsg::Delta {
            call: CallId::new(1).unwrap(),
            text: "x".repeat(BUDGET.bytes / 3),
            attempt: 0,
        })
        .unwrap();
        for _ in 0..4 {
            outbox.send(big.clone()).unwrap();
        }
        assert_eq!(written(&outbox).len(), 1);
    }

    #[test]
    fn switching_sessions_drains_each_certified_prefix_before_the_next_session() {
        let mut old = log(&["old one", "old two", "old three"]);
        let next = log(&["new one", "new two"]);
        let outbox = Outbox::new();
        let first = outbox.follow(
            old.frames(),
            0,
            vec![
                (2, Frame::new(&DaemonMsg::Synced { seq: 2 }).unwrap()),
                (
                    3,
                    Frame::new(&DaemonMsg::Activity {
                        seq: 3,
                        activity: protocol::Activity::Idle,
                    })
                    .unwrap(),
                ),
            ],
        );
        assert_eq!(outbox.try_next().unwrap().seq, Some(3));
        let second = outbox.follow(
            next.frames(),
            0,
            vec![(2, Frame::new(&DaemonMsg::Synced { seq: 2 }).unwrap())],
        );
        old.push("outside the old prefix");
        assert!(first.send(delta()).is_err());
        outbox.unfollow();
        assert!(second.send(delta()).is_err());
        assert_eq!(written(&outbox), [None, None, Some(2), None]);
    }

    #[test]
    fn a_session_no_longer_followed_is_refused() {
        let outbox = Outbox::new();
        let first = outbox.follow(log(&[]).frames(), 0, Vec::new());
        let second = outbox.follow(log(&[]).frames(), 0, Vec::new());
        assert!(first.send(entry(1, "a")).is_err());
        second.send(entry(1, "b")).unwrap();
        outbox.unfollow();
        assert!(second.send(entry(2, "c")).is_err());
        assert_eq!(written(&outbox), [Some(1)]);
    }

    #[test]
    fn a_closed_outbox_refuses_frames_and_writes_out_what_it_has() {
        let outbox = Outbox::new();
        outbox.send(delta()).unwrap();
        outbox.close();
        assert!(outbox.send(entry(1, "a")).is_err());
        assert!(rt::block_on(outbox.next(&rt::Threads::new(1))).is_some());
        assert!(rt::block_on(outbox.next(&rt::Threads::new(1))).is_none());
    }
}

#[cfg(test)]
mod scheduling {
    use super::*;
    use rt::Runtime;

    #[test]
    fn cold_replay_does_not_block_async_workers() {
        struct Cold {
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl Lines for Cold {
            fn read(
                &self,
                _: std::ops::RangeInclusive<u64>,
                _: usize,
                _: &mut Vec<u8>,
            ) -> io::Result<u64> {
                self.entered.send(()).unwrap();
                lock(&self.release).recv().unwrap();
                Err(io::Error::other("cold read failed"))
            }
        }
        let rt = Arc::new(rt::Threads::new(1));
        let (entered, reading) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let outbox = Outbox::new();
        let reader: Arc<dyn Lines> = Arc::new(Cold {
            entered,
            release: Mutex::new(released),
        });
        let log = Arc::new(FrameLog::new(Arc::clone(&reader), 1));
        let _follower = outbox.follow(Arc::clone(&log), 0, Vec::new());
        let runtime = Arc::clone(&rt);
        let read = rt::spawn(&*rt, async move { outbox.next(&*runtime).await });
        reading
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let (sent, responsive) = std::sync::mpsc::channel();
        rt.spawn(Box::pin(async move {
            log.reloaded(reader);
            sent.send(()).unwrap();
        }));
        let result = responsive.recv_timeout(std::time::Duration::from_secs(2));
        release.send(()).unwrap();
        result.unwrap();
        assert!(rt::block_on(read).is_none());
        rt.shutdown();
    }
}
