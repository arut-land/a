//! The session's one queue, as the kernel drains it: user input, other
//! sessions' messages, and the session's own envelopes (a child's result,
//! a detached call's outcome, a background job's end) wait on it in
//! arrival order, and at each [`Boundary`] the [`point::Dequeue`] point
//! says what to [`Take`].
//!
//! The kernel keeps only the boundaries: they are the places another
//! message can join the conversation without coming between a reply's
//! calls and their results. Which items go there is the point's, and with
//! no handler [`Take::default_at`] answers as the kernel always has.
//!
//! [`point::Dequeue`]: crate::point::Dequeue

use protocol::ledger::Queued;
use protocol::{CallId, Seq, Source};
use smallvec::SmallVec;

/// Where queued items may be taken up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// Call `call` of the reply just ended: an item taken here goes into
    /// its result ([`Take::IntoResult`]), so every call stays beside its
    /// result.
    ToolDone(CallId),
    /// The model is about to be asked again: what is taken is read at the
    /// tail of the request, so the provider's cache of the prefix holds.
    BeforeModel,
    /// The turn ended: what is taken starts the next one.
    TurnEnd,
}

/// What a dequeue takes, of the items in the queue's order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Take {
    #[default]
    None,
    /// The first.
    One,
    All,
    These(SmallVec<[Seq; 4]>),
    /// These, into the result of the call [`Boundary::ToolDone`] names:
    /// the one answer that takes anything there, and only there.
    IntoResult(SmallVec<[Seq; 4]>),
}

/// The queue as a dequeue sees it.
#[derive(Clone, Copy, Debug)]
pub struct QueueView<'a>(&'a [Queued]);

impl<'a> QueueView<'a> {
    pub const fn new(items: &'a [Queued]) -> Self {
        Self(items)
    }

    pub const fn items(self) -> &'a [Queued] {
        self.0
    }
}

impl Take {
    /// The answer with no handler, as the kernel took items before the
    /// point existed. Nothing goes into a result. Before a model call,
    /// steers and children's results are read, and so are messages and
    /// background outcomes, unless the user's input is queued ahead of
    /// them, which starts its own turn first. At a turn's end the first
    /// input or message starts the next turn, and only with none queued
    /// does the first of the session's own results (a child's, a
    /// background call's), so none jumps ahead of the user.
    pub fn default_at(boundary: Boundary, queue: QueueView<'_>) -> Self {
        let items = queue.items();
        match boundary {
            Boundary::ToolDone(_) => Self::None,
            Boundary::BeforeModel => {
                let ahead = items
                    .iter()
                    .position(|q| q.source.visible())
                    .unwrap_or(items.len());
                let these: SmallVec<_> = (items.iter().enumerate())
                    .filter(|&(at, q)| {
                        q.steer || matches!(q.source, Source::Child(_)) || at < ahead
                    })
                    .map(|(_, q)| q.seq)
                    .collect();
                if these.is_empty() {
                    Self::None
                } else {
                    Self::These(these)
                }
            }
            Boundary::TurnEnd => {
                let own = |q: &&Queued| matches!(q.source, Source::Child(_) | Source::Job(_));
                let first = (items.iter().find(|q| !own(q)))
                    .or_else(|| items.first())
                    .map(|q| q.seq);
                first.map_or(Self::None, |seq| Self::These(SmallVec::from_elem(seq, 1)))
            }
        }
    }

    /// The items it takes at `boundary`, in the queue's order: those it
    /// names that are queued, and nothing an answer cannot take there.
    pub fn of(&self, boundary: Boundary, queue: QueueView<'_>) -> SmallVec<[Seq; 4]> {
        let items = queue.items();
        let named = |these: &[Seq]| {
            items
                .iter()
                .filter(|q| these.contains(&q.seq))
                .map(|q| q.seq)
                .collect()
        };
        match (self, boundary) {
            (Self::IntoResult(these), Boundary::ToolDone(_)) => named(these),
            (_, Boundary::ToolDone(_)) | (Self::IntoResult(_) | Self::None, _) => SmallVec::new(),
            (Self::One, _) => items.iter().take(1).map(|q| q.seq).collect(),
            (Self::All, _) => items.iter().map(|q| q.seq).collect(),
            (Self::These(these), _) => named(these),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SessionId;

    fn item(seq: u64, steer: bool, source: Source) -> Queued {
        Queued {
            seq: Seq::new(seq).unwrap(),
            text: String::new(),
            steer,
            source,
            outcome: false,
            typed: Default::default(),
        }
    }

    fn seqs(of: &[u64]) -> SmallVec<[Seq; 4]> {
        of.iter().map(|&n| Seq::new(n).unwrap()).collect()
    }

    #[test]
    fn the_default_never_puts_an_item_between_a_reply_and_its_results() {
        let queue = [item(1, false, Source::Agent(SessionId::new()))];
        let done = Boundary::ToolDone(CallId::new(9).unwrap());
        let take = Take::default_at(done, QueueView::new(&queue));
        assert_eq!(take.of(done, QueueView::new(&queue)), seqs(&[]));
        // Only an answer naming the result takes anything there.
        let all = Take::All.of(done, QueueView::new(&queue));
        assert_eq!(all.as_slice(), []);
        let into = Take::IntoResult(seqs(&[1])).of(done, QueueView::new(&queue));
        assert_eq!(into, seqs(&[1]));
        let elsewhere = Take::IntoResult(seqs(&[1])).of(Boundary::TurnEnd, QueueView::new(&queue));
        assert_eq!(elsewhere.as_slice(), []);
    }

    #[test]
    fn before_a_model_call_the_default_reads_what_the_users_queued_input_does_not_hold_back() {
        let agent = Source::Agent(SessionId::new());
        let child = Source::Child(Seq::new(3).unwrap());
        let job = Source::Job(CallId::new(4).unwrap());
        let queue = [
            item(5, false, agent),
            item(6, false, job),
            item(7, false, Source::User),
            item(8, false, agent),
            item(9, true, Source::User),
            item(10, false, child),
        ];
        let view = QueueView::new(&queue);
        let take = Take::default_at(Boundary::BeforeModel, view);
        assert_eq!(take.of(Boundary::BeforeModel, view), seqs(&[5, 6, 9, 10]));
        let take = Take::default_at(Boundary::TurnEnd, view);
        assert_eq!(take.of(Boundary::TurnEnd, view), seqs(&[5]));
        let none = Take::default_at(Boundary::TurnEnd, QueueView::new(&[]));
        assert_eq!(none, Take::None);
    }

    #[test]
    fn at_a_turns_end_the_default_takes_the_users_input_before_the_sessions_own_results() {
        let child = Source::Child(Seq::new(3).unwrap());
        let job = Source::Job(CallId::new(4).unwrap());
        let at_end = |queue: &[Queued]| {
            let view = QueueView::new(queue);
            Take::default_at(Boundary::TurnEnd, view).of(Boundary::TurnEnd, view)
        };
        let queue = [
            item(5, false, child),
            item(6, false, job),
            item(7, false, Source::User),
        ];
        assert_eq!(at_end(&queue), seqs(&[7]));
        assert_eq!(at_end(&queue[..2]), seqs(&[5]));
        let agent = Source::Agent(SessionId::new());
        assert_eq!(
            at_end(&[item(5, false, child), item(6, false, agent)]),
            seqs(&[6])
        );
    }
}
