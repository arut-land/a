//! A connection's outgoing frames. A client following a session is a
//! cursor into the session's [`FrameLog`], which every client shares: a
//! slow viewer only lags, costs the daemon no copy of what it missed, and
//! never holds up a session. What is not an entry waits in a queue, each
//! frame for the entries before it; ephemeral frames are dropped once too
//! many wait.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, PoisonError, RwLock};

use protocol::{Caps, Class, Frame};

use crate::lock;

/// How many bytes of ephemeral frames may wait for a client: past that,
/// it gets the committed outcome without the deltas.
const BUDGET: usize = 1 << 20;

/// The frames of a session's committed entries, each encoded once as it
/// committed or loaded: entry `seq`'s is at `seq - 1`. `None` stands for
/// one too large for a frame, which ends the view of a client that comes
/// to it rather than leaving a gap in it.
#[derive(Default)]
pub struct FrameLog(RwLock<Vec<Option<Frame>>>);

impl FrameLog {
    /// The seq of the last entry committed.
    pub fn seq(&self) -> u64 {
        self.read().len() as u64
    }

    pub fn push(&self, frame: Option<Frame>) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(frame);
    }

    /// Entry `seq`'s frame, if it is committed.
    fn get(&self, seq: u64) -> Option<Result<Frame, TooLarge>> {
        let at = usize::try_from(seq.checked_sub(1)?).ok()?;
        self.read()
            .get(at)
            .map(|frame| frame.clone().ok_or(TooLarge))
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Option<Frame>>> {
        self.0.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An entry too large for a frame.
struct TooLarge;

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
    ready: Condvar,
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
}

/// The connection is gone, or follows another session.
#[derive(Debug)]
pub struct Closed;

impl Outbox {
    pub fn new(caps: Caps) -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::default(),
                ready: Condvar::new(),
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
        self.shared.ready.notify_one();
        Ok(())
    }

    /// Wakes the writer: entries committed.
    pub fn wake(&self) -> Result<(), Closed> {
        let q = self.queue();
        if q.closed || (self.following != 0 && self.following != q.follows) {
            return Err(Closed);
        }
        self.shared.ready.notify_one();
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
        q.follow = Some(Follow { log, cursor });
        q.frames.extend(frames);
        self.shared.ready.notify_one();
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
    /// all written.
    pub fn next(&self) -> Option<Frame> {
        let mut q = self.queue();
        loop {
            if let Some(frame) = q.step() {
                return Some(frame);
            }
            if q.closed {
                return None;
            }
            q = self
                .shared
                .ready
                .wait(q)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The next frame to write if there is one now.
    #[cfg(test)]
    pub fn try_next(&self) -> Option<Frame> {
        self.queue().step()
    }

    /// Ends the connection once what is queued is written: sends fail from
    /// now on.
    pub fn close(&self) {
        self.queue().closed = true;
        self.shared.ready.notify_all();
    }

    /// Ends the connection now, dropping what is queued: the writer failed.
    pub fn abandon(&self) {
        let mut q = self.queue();
        q.closed = true;
        q.follow = None;
        q.frames.clear();
        q.ephemeral = 0;
        self.shared.ready.notify_all();
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
        if let Ok(frame) = follow.log.get(follow.cursor + 1)? {
            follow.cursor += 1;
            return Some(frame);
        }
        // Too large for a frame: the view ends here.
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
    use protocol::{Body, DaemonMsg, Entry};

    use super::*;

    fn entry(seq: u64, text: &str) -> Frame {
        Frame::entry(&Entry::new(seq, 0, Body::User { text: text.into() })).unwrap()
    }

    fn delta() -> Frame {
        Frame::new(&DaemonMsg::Delta {
            call: 1,
            text: "t".into(),
        })
        .unwrap()
    }

    fn log(frames: impl IntoIterator<Item = Frame>) -> Arc<FrameLog> {
        let log = Arc::new(FrameLog::default());
        for frame in frames {
            log.push(Some(frame));
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
        let log = log([entry(1, "a"), entry(2, "b")]);
        let outbox = Outbox::new(Caps::NONE);
        let synced = Frame::new(&DaemonMsg::Synced { seq: 2 }).unwrap();
        let session = outbox.follow(Arc::clone(&log), 1, vec![(2, synced)]);
        outbox.send(delta()).unwrap();
        assert_eq!(written(&outbox), [Some(2), None, None]);
        log.push(Some(entry(3, "c")));
        session.wake().unwrap();
        assert_eq!(written(&outbox), [Some(3)]);
    }

    #[test]
    fn a_slow_client_costs_no_copy_and_loses_only_deltas() {
        let log = log([]);
        let outbox = Outbox::new(Caps::NONE);
        let session = outbox.follow(Arc::clone(&log), 0, Vec::new());
        let big = "x".repeat(BUDGET / 4);
        for seq in 1..=8 {
            log.push(Some(entry(seq, &big)));
            session.send_after(seq, delta()).unwrap();
        }
        let delta_len = delta().as_bytes().len();
        assert!(outbox.queue().ephemeral <= 8 * delta_len);
        let seqs = written(&outbox);
        assert_eq!(seqs.iter().filter(|s| s.is_some()).count(), 8);
    }

    #[test]
    fn deltas_past_the_budget_are_dropped() {
        let outbox = Outbox::new(Caps::NONE);
        let big = Frame::new(&DaemonMsg::Delta {
            call: 1,
            text: "x".repeat(BUDGET / 2),
        })
        .unwrap();
        for _ in 0..4 {
            outbox.send(big.clone()).unwrap();
        }
        assert_eq!(written(&outbox).len(), 1);
    }

    #[test]
    fn an_entry_too_large_for_a_frame_ends_the_view() {
        let log = log([entry(1, "a")]);
        log.push(None);
        log.push(Some(entry(3, "c")));
        let outbox = Outbox::new(Caps::NONE);
        let session = outbox.follow(log, 0, Vec::new());
        assert_eq!(written(&outbox), [Some(1)]);
        assert!(session.send(delta()).is_err());
    }

    #[test]
    fn a_session_no_longer_followed_is_refused() {
        let outbox = Outbox::new(Caps::NONE);
        let first = outbox.follow(log([]), 0, Vec::new());
        let second = outbox.follow(log([]), 0, Vec::new());
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
        assert!(outbox.next().is_some());
        assert!(outbox.next().is_none());
    }
}
