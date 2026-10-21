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
use protocol::{Class, Decode, Entry, Frame};

use crate::lock;

/// How many bytes of ephemeral frames may wait for a client: past that,
/// it gets the committed outcome without the deltas.
const BUDGET: usize = 1 << 20;

/// How many bytes of the newest entries' frames are kept in memory, for
/// the followers that keep up: a commit costs them no read of the log.
pub const TAIL: usize = 64 << 10;

/// A session's committed entries, shown to its clients: the lines its log
/// stores, each encoded once as it committed, read back by seq as a client
/// comes to it. The newest stay in a bounded tail, shared by every
/// follower; the rest stay where the log keeps them (for a file, the OS's
/// page cache), not in the daemon's memory.
pub struct FrameLog {
    lines: Mutex<Arc<dyn Lines>>,
    /// The seq of the last entry shown. Entries are shown under the lock
    /// they commit under, after the log holds them.
    seq: AtomicU64,
    tail: Mutex<Tail>,
}

/// The frames of the newest entries, contiguous up to the last shown.
#[derive(Default)]
struct Tail {
    frames: VecDeque<Frame>,
    bytes: usize,
}

impl FrameLog {
    /// The log of `lines`, whose entries up to `seq` are committed.
    pub fn new(lines: Arc<dyn Lines>, seq: u64) -> Self {
        Self {
            lines: Mutex::new(lines),
            seq: AtomicU64::new(seq),
            tail: Mutex::default(),
        }
    }

    /// A reload replaces the reader after taking a new writer lease.
    pub fn reloaded(&self, lines: Arc<dyn Lines>) {
        *lock(&self.lines) = lines;
    }

    /// The seq of the last entry committed.
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// Shows entry `seq`, the next, which the log holds already, as
    /// `frame`; `None` for one not framed, too large for the tail or for
    /// a frame at all, which followers come to in the log.
    pub fn push(&self, seq: u64, frame: Option<Frame>) {
        let mut tail = lock(&self.tail);
        match frame {
            Some(frame) if frame.as_bytes().len() <= TAIL => {
                tail.bytes += frame.as_bytes().len();
                tail.frames.push_back(frame);
                while tail.bytes > TAIL
                    && let Some(old) = tail.frames.pop_front()
                {
                    tail.bytes -= old.as_bytes().len();
                }
            }
            _ => *tail = Tail::default(),
        }
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
            Some(match self.batch(cursor + 1, last) {
                Ok((_, entries)) => {
                    cursor += entries.len() as u64;
                    Ok(entries)
                }
                Err(error) => {
                    cursor = last;
                    Err(error)
                }
            })
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

    fn batch(&self, next: u64, last: u64) -> io::Result<(Vec<u8>, Vec<Entry>)> {
        let mut run = Vec::new();
        let count = lock(&self.lines).read(next..=last, RUN, &mut run)?;
        if count == 0 || count > last - next + 1 || run.last() != Some(&b'\n') {
            return Err(io::Error::other("log read made no valid progress"));
        }
        let entries: Vec<_> = lines(&run)
            .map(|line| Entry::from_json(line).map_err(io::Error::other))
            .collect::<io::Result<_>>()?;
        if entries.len() as u64 != count
            || entries
                .iter()
                .zip(next..)
                .any(|(entry, seq)| entry.seq.get() != seq)
        {
            return Err(io::Error::other("log batch is not contiguous"));
        }
        Ok((run, entries))
    }

    /// Queues the frames of the entries after `after`: from the tail when
    /// it holds them, else what one read of the log gets; none if there
    /// are no more. One too large for a frame, or that cannot be read
    /// back, is an error that ends the view of a client that comes to it
    /// rather than leaving a gap in it.
    fn read_after(&self, after: u64, ahead: &mut VecDeque<io::Result<Frame>>) {
        let last = {
            // The seq read under the lock it is shown under: the tail's
            // frames end at it, where a seq read before may be behind them.
            let tail = lock(&self.tail);
            let last = self.seq();
            if after >= last {
                return;
            }
            let first = last + 1 - tail.frames.len() as u64;
            if first <= after + 1 {
                let skip = usize::try_from(after + 1 - first).unwrap_or(usize::MAX);
                ahead.extend(tail.frames.iter().skip(skip).cloned().map(Ok));
                return;
            }
            last
        };
        match self.batch(after + 1, last) {
            Ok((run, _)) => ahead.extend(
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
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::default(),
                ready: rt::Notify::new(),
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
            if let Some(frame) = self.try_next() {
                return Some(frame);
            }
            if self.queue().closed {
                return None;
            }
            self.shared.ready.changed(&mut seen).await;
        }
    }

    /// The next frame to write if there is one now. The log is read
    /// outside the queue's lock: a cold read holds up no sender.
    pub fn try_next(&self) -> Option<Frame> {
        loop {
            let (log, cursor, follows) = match self.queue().step() {
                Step::Frame(frame) => return Some(frame),
                Step::Nothing => return None,
                Step::Read(log, cursor, follows) => (log, cursor, follows),
            };
            let mut ahead = VecDeque::new();
            log.read_after(cursor, &mut ahead);
            if ahead.is_empty() {
                return None;
            }
            let mut q = self.queue();
            // Still following the same session from the same place.
            if q.follows == follows
                && let Some(follow) = &mut q.follow
                && follow.cursor == cursor
            {
                follow.ahead = ahead;
            }
        }
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
        let cursor = self.follow.as_ref().map_or(u64::MAX, |f| f.cursor);
        if self
            .frames
            .front()
            .is_some_and(|&(after, _)| after <= cursor)
        {
            let Some((_, frame)) = self.frames.pop_front() else {
                return Step::Nothing;
            };
            if frame.class == Class::Ephemeral {
                self.ephemeral -= frame.as_bytes().len();
            }
            return Step::Frame(frame);
        }
        let Some(follow) = self.follow.as_mut() else {
            return Step::Nothing;
        };
        match follow.ahead.pop_front() {
            None => Step::Read(Arc::clone(&follow.log), follow.cursor, self.follows),
            Some(Ok(frame)) => {
                follow.cursor += 1;
                Step::Frame(frame)
            }
            // Queued snapshots and acknowledgements certify a prefix we cannot deliver.
            Some(Err(_)) => {
                self.closed = true;
                self.follow = None;
                self.frames.clear();
                self.ephemeral = 0;
                Step::Nothing
            }
        }
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
    use std::sync::atomic::AtomicUsize;

    use protocol::{Body, CallId, DaemonMsg, Epoch, Seq};

    use super::*;

    fn entry(seq: u64, text: &str) -> Frame {
        Frame::entry(&Entry::new(
            Seq::new(seq).unwrap(),
            Epoch(0),
            Body::Input {
                text: text.into(),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: None,
            },
        ))
        .unwrap()
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
                    text: text.into(),
                    mode: protocol::Mode::Queue,
                    taken: true,
                    origin: None,
                })
                .unwrap();
            let frame = (text.len() <= TAIL)
                .then(|| Frame::entry(&e).ok())
                .flatten();
            self.1.push(e.seq.get(), frame);
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
    struct Counted(Arc<dyn Lines>, AtomicUsize);

    impl Lines for Counted {
        fn read(
            &self,
            seqs: std::ops::RangeInclusive<u64>,
            limit: usize,
            out: &mut Vec<u8>,
        ) -> io::Result<u64> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.read(seqs, limit, out)
        }
    }

    /// Followers that keep up are served the frames the commit encoded,
    /// however many follow: no read of the log per client per entry. Only
    /// one that comes from far behind reads it, once per run.
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
            let outbox = Outbox::new();
            let follower = outbox.follow(
                frames,
                0,
                vec![(3, Frame::new(&DaemonMsg::Synced { seq: 3 }).unwrap())],
            );
            assert_eq!(written(&outbox), [Some(1)]);
            assert!(follower.wake().is_err());
        }
    }

    #[test]
    fn a_live_follower_reads_no_file() {
        let mut log = log(&[]);
        let counted = Arc::new(Counted(Arc::clone(&log.0.lines), AtomicUsize::new(0)));
        log.1 = Arc::new(FrameLog::new(counted.clone(), 0));
        let (near, far) = (Outbox::new(), Outbox::new());
        let live = near.follow(log.frames(), 0, Vec::new());
        for i in 0..64 {
            log.push(&format!("{i}"));
            live.wake().unwrap();
            assert_eq!(written(&near).len(), 1);
        }
        assert_eq!(counted.1.load(Ordering::SeqCst), 0);
        // Past the tail: read from the log, from where the tail starts on.
        let big = "x".repeat(TAIL);
        log.push(&big);
        log.push("after");
        let _ = far.follow(log.frames(), 0, Vec::new());
        assert_eq!(written(&far).len(), 66);
        assert!(counted.1.load(Ordering::SeqCst) >= 1);
        // The entry too large for the tail is read, once, by whoever
        // comes to it; what follows it is in the tail again.
        let reads = counted.1.load(Ordering::SeqCst);
        live.wake().unwrap();
        assert_eq!(written(&near).len(), 2);
        assert_eq!(counted.1.load(Ordering::SeqCst), reads + 1);
        log.push("more");
        live.wake().unwrap();
        assert_eq!(written(&near).len(), 1);
        assert_eq!(counted.1.load(Ordering::SeqCst), reads + 1);
    }

    /// Followers reading while entries commit get each once, in order:
    /// where a read starts in the tail is reckoned from the seq the tail's
    /// frames end at, never from one shown before.
    #[test]
    fn followers_racing_the_commits_get_every_entry_once_in_order() {
        const N: usize = 100_000;
        let mut log = log(&[]);
        let followers: Vec<_> = (0..4)
            .map(|_| {
                let outbox = Outbox::new();
                let _ = outbox.follow(log.frames(), 0, Vec::new());
                std::thread::spawn(move || {
                    let mut seqs = Vec::new();
                    while seqs.len() < N {
                        match outbox.try_next() {
                            Some(frame) => seqs.push(frame.seq.unwrap()),
                            None => std::hint::spin_loop(),
                        }
                    }
                    // The first entry out of place, and the seq it holds.
                    (1..).zip(seqs).find(|&(want, seq)| seq != want)
                })
            })
            .collect();
        for _ in 0..N {
            log.push("x");
        }
        for follower in followers {
            assert_eq!(follower.join().unwrap(), None);
        }
    }

    #[test]
    fn a_follower_reads_the_shared_log_from_its_cursor_and_frames_wait_for_their_entry() {
        let mut log = log(&["a", "b"]);
        let outbox = Outbox::new();
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
        let outbox = Outbox::new();
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
        let outbox = Outbox::new();
        let big = Frame::new(&DaemonMsg::Delta {
            call: CallId::new(1).unwrap(),
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
        let outbox = Outbox::new();
        let session = outbox.follow(log.frames(), 0, Vec::new());
        assert_eq!(written(&outbox), [Some(1)]);
        assert!(session.send(delta()).is_err());
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
        assert!(rt::block_on(outbox.next()).is_some());
        assert!(rt::block_on(outbox.next()).is_none());
    }
}
