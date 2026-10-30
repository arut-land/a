use protocol::history::Cut;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OrderedCut(Cut);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DurableCut(Cut);

impl OrderedCut {
    pub(super) fn new(cut: Cut) -> Self {
        Self(cut)
    }

    pub(crate) fn seq(self) -> u64 {
        self.0.seq
    }

    pub(super) fn advance(self, seq: u64) -> Self {
        Self(Cut { seq, ..self.0 })
    }

    pub(super) fn covers(self, other: Self) -> bool {
        self.0.session == other.0.session
            && self.0.generation == other.0.generation
            && self.0.seq >= other.0.seq
    }
}

impl DurableCut {
    // Only the journal resolves flush epochs and can strengthen this guarantee.
    pub(super) fn certify(cut: OrderedCut) -> Self {
        Self(cut.0)
    }

    pub(crate) fn seq(self) -> u64 {
        self.0.seq
    }

    pub(super) fn ordered(self) -> OrderedCut {
        OrderedCut(self.0)
    }

    pub(crate) fn covers(self, cut: OrderedCut) -> bool {
        self.ordered().covers(cut)
    }

    pub(crate) fn prefix(self, seq: u64) -> Option<Self> {
        (seq <= self.0.seq).then_some(Self(Cut { seq, ..self.0 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Epoch, SessionId};

    #[test]
    fn coverage_never_crosses_session_or_generation() {
        let cut = Cut {
            session: SessionId::new(),
            generation: Epoch(1),
            seq: 4,
        };
        let durable = DurableCut::certify(OrderedCut::new(cut));
        assert!(durable.covers(OrderedCut::new(cut)));
        assert!(durable.covers(OrderedCut::new(Cut { seq: 2, ..cut })));
        assert!(!durable.covers(OrderedCut::new(Cut { seq: 5, ..cut })));
        assert!(!durable.covers(OrderedCut::new(Cut {
            generation: Epoch(2),
            seq: 2,
            ..cut
        })));
        assert!(!durable.covers(OrderedCut::new(Cut {
            session: SessionId::new(),
            seq: 2,
            ..cut
        })));
    }

    #[test]
    fn a_durable_prefix_cannot_certify_a_later_position() {
        let cut = Cut {
            session: SessionId::new(),
            generation: Epoch(1),
            seq: 4,
        };
        let durable = DurableCut::certify(OrderedCut::new(cut));
        assert_eq!(durable.prefix(2).unwrap().0, Cut { seq: 2, ..cut });
        assert_eq!(durable.prefix(5), None);
    }
}
