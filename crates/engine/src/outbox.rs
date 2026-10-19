//! A connection's outgoing frames. A client following a session is a
//! cursor into the session's [`FrameLog`], which every client shares: a
//! slow viewer only lags, costs the daemon no copy of what it missed, and
//! never holds up a session. What is not an entry waits in a queue, each
//! frame for the entries before it; ephemeral frames are dropped once too
//! many wait.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::store::Lines;
use protocol::{Caps, Class, Decode, Entry, Frame};

use crate::lock;

/// How many bytes of ephemeral frames may wait for a client: past that,
/// it gets the committed outcome without the deltas.
const BUDGET: usize = 1 << 20;

/// A session's committed entries, shown to its clients: the lines its log
/// stores, each encoded once as it committed, read back by seq as a client
/// comes to it. The bytes stay where the log keeps them (for a file, the
/// OS's page cache), not in the daemon's memory.
pub struct FrameLog {
    lines: Arc<dyn Lines>,
    /// The seq of the last entry shown. Entries are shown under the lock
    /// they commit under, after the log holds them.
    seq: AtomicU64,
}

impl FrameLog {
    /// The log of `lines`, whose entries up to `seq` are committed.
    pub fn new(lines: Arc<dyn Lines>, seq: u64) -> Self {
        Self {
            lines,
            seq: AtomicU64::new(seq),
        }
    }

    /// The seq of the last entry committed.
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Shows entry `seq`, the next, which the log holds already.
    pub fn push(&self, seq: u64) {
        let last = self.seq.swap(seq, Ordering::Release);
        debug_assert_eq!(last + 1, seq, "entries are shown in order");
    }

    /// The entries after `seq`, decoded from their lines a run at a time,
    /// in order: for what reads the log rarely, such as recovery after a
    /// failure.
    pub fn entries_after(&self, seq: u64) -> impl Iterator<Item = Entry> + '_ {
        let (mut next, last) = (seq + 1, self.seq());
        let (mut run, mut read) = (Vec::new(), VecDeque::new());
        std::iter::from_fn(move || {
            loop {
                if let Some(entry) = read.pop_front() {
                    return Some(entry);
                }
                if next > last {
                    return None;
                }
                run.clear();
                next += self.lines.read(next..=last, RUN, &mut run).ok()?;
                read.extend(lines(&run).filter_map(|line| Entry::from_json(line).ok()));
            }
        })
    }

    pub fn entries(&self) -> impl Iterator<Item = Entry> + '_ {
        self.entries_after(0)
    }

    /// Queues the frames of the entries after `after` that one read of the
    /// log gets, none if there are no more. One too large for a frame, or
    /// that cannot be read back, is an error that ends the view of a client
    /// that comes to it rather than leaving a gap in it.
    fn read_after(&self, after: u64, ahead: &mut VecDeque<io::Result<Frame>>) {
        let last = self.seq();
        if after >= last {
            return;
        }
        let mut run = Vec::new();
        match self.lines.read(after + 1..=last, RUN, &mut run) {
            Ok(_) => ahead.extend(
                lines(&run)
                    .zip(after + 1..)
                    .map(|(line, seq)| Frame::from_line(seq, line)),
            ),
            Err(e) => ahead.push_back(Err(e)),
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

impl ext::History for FrameLog {
    fn entries(&self) -> Box<dyn Iterator<Item = Entry> + '_> {
        Box::new(self.entries_after(0))
    }
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

struct Shared {
    queue: Mutex<Queue>,
    /// Notified as there may be more to write: the writer's one wake.
    ready: rt::Notify,
    caps: Caps,
}

#[derive(Default)]
struct Queue {
    /// Frames to write once the client has every entry up to the seq each
    /// waits for, in order.
    frames: VecDeque<(u64, Frame)>,
    /// Bytes of ephemeral frames queued.
    ephemeral: usize,
    follow: Option<Follow>,
    /// The number of the last session followed.
    follows: u64,
    /// The writer is gone.
    closed: bool,
}

/// The session a client follows, and how far it has read its entries.
struct Follow {
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
    pub fn new(caps: Caps) -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::default(),
                ready: rt::Notify::new(),
                caps,
            }),
            following: 0,
        }
    }

    /// The capabilities the client took up.
    pub fn caps(&self) -> Caps {
        self.shared.caps
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        lock(&self.shared.queue)
    }

    /// Queues `frame` behind every entry committed so far.
    pub fn send(&self, frame: Frame) -> Result<(), Closed> {
        let mut q = self.queue();
        let after = q.follow.as_ref().map_or(0, |f| f.log.seq());
        self.push(&mut q, after, frame)
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
        if frame.class == Class::Ephemeral {
            let len = frame.as_bytes().len();
            if q.ephemeral + len > BUDGET {
                return Ok(());
            }
            q.ephemeral += len;
        }
        q.frames.push_back((after, frame));
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
    pub fn follow(&self, log: Arc<FrameLog>, cursor: u64, frames: Vec<(u64, Frame)>) -> Self {
        let mut q = self.queue();
        q.unfollow();
        q.follow = Some(Follow {
            log,
            cursor,
            ahead: VecDeque::new(),
        });
        q.frames.extend(frames);
        self.shared.ready.notify();
        Self {
            shared: Arc::clone(&self.shared),
            following: q.follows,
        }
    }

    /// Stops following a session.
    pub fn unfollow(&self) {
        self.queue().unfollow();
    }

    /// The next frame to write, once there is one; `None` once closed and
    /// all written. The writer waits here, a task: no thread per client.
    pub async fn next(&self) -> Option<Frame> {
        loop {
            let mut seen = self.shared.ready.count();
            {
                let mut q = self.queue();
                if let Some(frame) = q.step() {
                    return Some(frame);
                }
                if q.closed {
                    return None;
                }
            }
            self.shared.ready.changed(&mut seen).await;
        }
    }

    /// The next frame to write if there is one now.
    pub fn try_next(&self) -> Option<Frame> {
        self.queue().step()
    }

    /// Ends the connection once what is queued is written: sends fail from
    /// now on.
    pub fn close(&self) {
        self.queue().closed = true;
        self.shared.ready.notify();
    }

    /// Ends the connection now, dropping what is queued: the writer failed.
    pub fn abandon(&self) {
        let mut q = self.queue();
        q.closed = true;
        q.follow = None;
        q.frames.clear();
        q.ephemeral = 0;
        self.shared.ready.notify();
    }
}

impl Queue {
    /// The next frame to write, if there is one now: the next queued if the
    /// client has the entries it waits for, else the next entry.
    fn step(&mut self) -> Option<Frame> {
        let cursor = self.follow.as_ref().map_or(u64::MAX, |f| f.cursor);
        if self
            .frames
            .front()
            .is_some_and(|&(after, _)| after <= cursor)
        {
            let (_, frame) = self.frames.pop_front()?;
            if frame.class == Class::Ephemeral {
                self.ephemeral -= frame.as_bytes().len();
            }
            return Some(frame);
        }
        let follow = self.follow.as_mut()?;
        if follow.ahead.is_empty() {
            follow.log.read_after(follow.cursor, &mut follow.ahead);
        }
        if let Ok(frame) = follow.ahead.pop_front()? {
            follow.cursor += 1;
            return Some(frame);
        }
        // Too large for a frame, or unreadable: the view ends here.
        self.unfollow();
        self.step()
    }

    /// What was queued for the session followed goes out first, as it is.
    fn unfollow(&mut self) {
        self.follows += 1;
        self.follow = None;
        for (after, _) in &mut self.frames {
            *after = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use protocol::{Body, DaemonMsg};

    use super::*;

    fn entry(seq: u64, text: &str) -> Frame {
        Frame::entry(&Entry::new(seq, 0, Body::User { text: text.into() })).unwrap()
    }

    fn delta() -> Frame {
        Frame::new(&DaemonMsg::Delta {
            call: 1,
            text: "t".into(),
            attempt: 0,
        })
        .unwrap()
    }

    /// A session's log, and the frame log that shows it.
    struct Shown(crate::store::Loaded, Arc<FrameLog>);

    impl Shown {
        fn push(&mut self, text: &str) {
            let e = self.0.append(Body::User { text: text.into() }).unwrap();
            self.1.push(e.seq);
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
            extensions: Vec::new(),
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

    #[test]
    fn a_follower_reads_the_shared_log_from_its_cursor_and_frames_wait_for_their_entry() {
        let mut log = log(&["a", "b"]);
        let outbox = Outbox::new(Caps::NONE);
        let synced = Frame::new(&DaemonMsg::Synced { seq: 2 }).unwrap();
        let session = outbox.follow(log.frames(), 1, vec![(2, synced)]);
        outbox.send(delta()).unwrap();
        assert_eq!(written(&outbox), [Some(2), None, None]);
        log.push("c");
        session.wake().unwrap();
        assert_eq!(written(&outbox), [Some(3)]);
    }

    #[test]
    fn a_slow_viewer_never_grows_daemon_memory_beyond_its_cursor() {
        let mut log = log(&[]);
        let outbox = Outbox::new(Caps::NONE);
        let session = outbox.follow(log.frames(), 0, Vec::new());
        let big = "x".repeat(BUDGET);
        for seq in 1..=64 {
            log.push(&big);
            session.wake().unwrap();
            session.send_after(seq, delta()).unwrap();
        }
        // Entries stay in the shared log: what waits for this viewer is
        // its cursor and the bounded deltas, however far it lags.
        let q = outbox.queue();
        assert!(q.frames.iter().all(|(_, f)| f.class == Class::Ephemeral));
        assert!(q.ephemeral <= BUDGET);
        assert_eq!(q.follow.as_ref().map(|f| f.cursor), Some(0));
        drop(q);
        let seqs = written(&outbox);
        assert_eq!(seqs.iter().filter(|s| s.is_some()).count(), 64);
    }

    #[test]
    fn deltas_past_the_budget_are_dropped() {
        let outbox = Outbox::new(Caps::NONE);
        let big = Frame::new(&DaemonMsg::Delta {
            call: 1,
            text: "x".repeat(BUDGET / 2),
            attempt: 0,
        })
        .unwrap();
        for _ in 0..4 {
            outbox.send(big.clone()).unwrap();
        }
        assert_eq!(written(&outbox).len(), 1);
    }

    #[test]
    fn an_entry_too_large_for_a_frame_ends_the_view() {
        let mut log = log(&["a"]);
        log.push(&"x".repeat(16 << 20));
        log.push("c");
        let outbox = Outbox::new(Caps::NONE);
        let session = outbox.follow(log.frames(), 0, Vec::new());
        assert_eq!(written(&outbox), [Some(1)]);
        assert!(session.send(delta()).is_err());
    }

    #[test]
    fn a_session_no_longer_followed_is_refused() {
        let outbox = Outbox::new(Caps::NONE);
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
        let outbox = Outbox::new(Caps::NONE);
        outbox.send(delta()).unwrap();
        outbox.close();
        assert!(outbox.send(entry(1, "a")).is_err());
        assert!(rt::block_on(outbox.next()).is_some());
        assert!(rt::block_on(outbox.next()).is_none());
    }
}
