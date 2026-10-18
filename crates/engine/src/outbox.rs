//! A connection's outgoing frames, bounded: a client too slow to keep up
//! loses its backlog and is caught up from the log instead (a replay from
//! the last entry it took, or one snapshot), so a stuck viewer costs the
//! daemon bounded memory and never holds up a session.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};

use protocol::{Caps, Class, Frame};

use crate::lock;
use crate::session::Session;

/// How many bytes of frames may wait for a client before it counts as
/// behind. Ephemeral frames are dropped from half of it.
const BUDGET: usize = 4 << 20;

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
    frames: VecDeque<Frame>,
    bytes: usize,
    /// Bytes of catch-up still queued, which the budget allows on top: the
    /// client is owed its backlog whatever its size.
    owed: usize,
    /// Durable frames were dropped: the client is caught up next.
    behind: bool,
    /// The writer is gone.
    closed: bool,
    /// The session followed, which catches the client up.
    session: Option<Weak<Session>>,
    /// The number of the last session followed.
    follows: u64,
    /// The seq of the last entry the writer took.
    written: u64,
}

/// The connection is gone, or follows another session.
#[derive(Debug)]
pub struct Closed;

/// What the writer does next.
pub enum Next {
    Write(Frame),
    /// Have `session`, the `following`th followed, catch the client up from
    /// entry `since`.
    CatchUp {
        session: Weak<Session>,
        following: u64,
        since: u64,
    },
}

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

    /// Queues `frame`. A client that is behind gets durable frames from its
    /// catch-up instead, so those are dropped, as are ephemeral ones.
    pub fn send(&self, frame: Frame) -> Result<(), Closed> {
        let mut q = self.queue();
        if q.closed || (self.following != 0 && self.following != q.follows) {
            return Err(Closed);
        }
        let used = q.bytes.saturating_sub(q.owed);
        let dropped = match frame.class {
            Class::Ephemeral => q.behind || used > BUDGET / 2,
            Class::Durable if q.behind => true,
            Class::Durable if used > BUDGET && q.session.is_some() => {
                q.fall_behind();
                true
            }
            Class::Durable | Class::Control => false,
        };
        if !dropped {
            q.push(frame);
        }
        self.shared.ready.notify_one();
        Ok(())
    }

    /// Follows `session`, in place of any followed before, from entry
    /// `since`, the last the client has; queues `frames` (the attach and
    /// its replay) whatever their size. Callers hold the session's state
    /// lock, so no entry commits meanwhile. The outbox returned is the
    /// session's.
    #[must_use]
    pub fn follow(&self, session: Weak<Session>, since: u64, frames: Vec<Frame>) -> Self {
        let mut q = self.queue();
        q.follows += 1;
        q.session = Some(session);
        q.written = since;
        q.behind = false;
        q.catch_up(frames);
        self.shared.ready.notify_one();
        Self {
            shared: Arc::clone(&self.shared),
            following: q.follows,
        }
    }

    /// Queues the catch-up of a client that fell behind the `following`th
    /// session it followed, whatever its size, unless it follows another
    /// by now. Callers hold the session's state lock, as for
    /// [`Outbox::follow`].
    pub fn caught_up(&self, following: u64, frames: Vec<Frame>) {
        let mut q = self.queue();
        if q.follows != following {
            return;
        }
        q.behind = false;
        q.catch_up(frames);
        self.shared.ready.notify_one();
    }

    /// Stops following a session.
    pub fn unfollow(&self) {
        let mut q = self.queue();
        q.follows += 1;
        q.session = None;
        q.behind = false;
    }

    /// The writer's next step, once there is one; `None` once closed.
    pub fn next(&self) -> Option<Next> {
        let mut q = self.queue();
        loop {
            if let Some(next) = q.step() {
                return Some(next);
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

    /// The writer's next step if there is one now.
    #[cfg(test)]
    pub fn try_next(&self) -> Option<Next> {
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
        q.frames.clear();
        q.bytes = 0;
        self.shared.ready.notify_all();
    }

    /// The frames queued, taken without waiting.
    #[cfg(test)]
    pub fn take(&self) -> Vec<Frame> {
        let mut q = self.queue();
        q.bytes = 0;
        q.frames.drain(..).collect()
    }
}

impl Queue {
    /// The writer's next step, if there is one now.
    fn step(&mut self) -> Option<Next> {
        if let Some(frame) = self.frames.pop_front() {
            let len = frame.as_bytes().len();
            self.bytes -= len;
            self.owed = self.owed.saturating_sub(len);
            if let Some(seq) = frame.seq {
                self.written = seq;
            }
            return Some(Next::Write(frame));
        }
        // Still behind until the catch-up is queued: whatever commits
        // meanwhile is in it.
        let session = self
            .session
            .clone()
            .filter(|_| self.behind && !self.closed)?;
        Some(Next::CatchUp {
            session,
            following: self.follows,
            since: self.written,
        })
    }

    fn push(&mut self, frame: Frame) {
        self.bytes += frame.as_bytes().len();
        self.frames.push_back(frame);
    }

    fn catch_up(&mut self, frames: Vec<Frame>) {
        for frame in frames {
            self.owed += frame.as_bytes().len();
            self.push(frame);
        }
    }

    /// Drops all but the control frames: the rest come again in the
    /// catch-up.
    fn fall_behind(&mut self) {
        self.behind = true;
        self.frames.retain(|f| f.class == Class::Control);
        self.bytes = self.frames.iter().map(|f| f.as_bytes().len()).sum();
        self.owed = 0;
    }
}

#[cfg(test)]
mod tests {
    use protocol::{Body, DaemonMsg, Entry};

    use super::*;

    fn entry(seq: u64, text: &str) -> Frame {
        Frame::entry(&Entry {
            seq,
            epoch: 0,
            body: Body::User { text: text.into() },
        })
        .unwrap()
    }

    fn delta() -> Frame {
        Frame::new(&DaemonMsg::Delta {
            call: 1,
            text: "t".into(),
        })
        .unwrap()
    }

    /// The seqs of the frames queued, as the writer takes them.
    fn written(outbox: &Outbox) -> Vec<Option<u64>> {
        let mut seqs = Vec::new();
        while !outbox.queue().frames.is_empty() {
            let Some(Next::Write(f)) = outbox.next() else {
                unreachable!()
            };
            seqs.push(f.seq);
        }
        seqs
    }

    #[test]
    fn a_client_too_far_behind_keeps_only_control_frames_and_is_caught_up_from_what_it_took() {
        let outbox = Outbox::new(Caps::NONE);
        let session = outbox.follow(Weak::new(), 0, vec![entry(1, "a")]);
        assert!(matches!(outbox.next(), Some(Next::Write(f)) if f.seq == Some(1)));
        let big = "x".repeat(BUDGET / 4);
        for seq in 2..8 {
            session.send(entry(seq, &big)).unwrap();
        }
        let error = Frame::new(&DaemonMsg::Error {
            message: "e".into(),
        })
        .unwrap();
        outbox.send(error).unwrap();
        session.send(delta()).unwrap();
        assert_eq!(written(&outbox), [None]);
        assert!(matches!(
            outbox.next(),
            Some(Next::CatchUp { since: 1, .. })
        ));
        outbox.caught_up(1, vec![entry(2, "b")]);
        session.send(entry(3, "c")).unwrap();
        assert_eq!(written(&outbox), [Some(2), Some(3)]);
    }

    #[test]
    fn a_catch_up_is_queued_whatever_its_size() {
        let outbox = Outbox::new(Caps::NONE);
        let big = "x".repeat(BUDGET);
        let session = outbox.follow(Weak::new(), 0, vec![entry(1, &big), entry(2, &big)]);
        session.send(entry(3, "c")).unwrap();
        assert_eq!(written(&outbox), [Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn ephemeral_frames_go_first() {
        let outbox = Outbox::new(Caps::NONE);
        let session = outbox.follow(Weak::new(), 0, Vec::new());
        session.send(entry(1, &"x".repeat(BUDGET / 2 + 1))).unwrap();
        session.send(delta()).unwrap();
        session.send(entry(2, "b")).unwrap();
        assert_eq!(written(&outbox), [Some(1), Some(2)]);
    }

    #[test]
    fn a_session_no_longer_followed_is_refused() {
        let outbox = Outbox::new(Caps::NONE);
        let first = outbox.follow(Weak::new(), 0, Vec::new());
        let second = outbox.follow(Weak::new(), 0, Vec::new());
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
        assert!(matches!(outbox.next(), Some(Next::Write(_))));
        assert!(outbox.next().is_none());
    }
}
