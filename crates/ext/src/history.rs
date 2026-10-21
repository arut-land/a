//! An immutable prefix validated by its shape and the installed history policy.

use crate::host::{Binding, HistoryPolicy};
use protocol::{Entry, ForkError, ForkParent, Seq, SessionId};
use std::sync::Arc;

#[derive(Debug)]
pub struct ValidatedPrefix {
    entries: Vec<Entry>,
    parent: ForkParent,
}

impl ValidatedPrefix {
    pub fn prepare(
        policy: &dyn HistoryPolicy,
        binding: Arc<dyn Binding>,
        entries: Vec<Entry>,
        source: SessionId,
        seq: Seq,
    ) -> Result<Self, ForkError> {
        if entries.last().map(|entry| entry.seq) != Some(seq)
            || entries
                .iter()
                .enumerate()
                .any(|(index, entry)| entry.seq.get() != index as u64 + 1)
        {
            return Err(ForkError::InvalidSequence);
        }
        policy.validate(binding, &entries)?;
        Ok(Self {
            entries,
            parent: ForkParent {
                session: source,
                seq,
            },
        })
    }

    pub fn into_history(self) -> (Vec<Entry>, ForkParent) {
        (self.entries, self.parent)
    }
}
