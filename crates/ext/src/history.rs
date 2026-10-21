//! An immutable prefix validated by its shape and the installed history policy.

use crate::Name;
use crate::host::{Binding, HistoryPolicy};
use protocol::{Entry, ForkError, ForkParent, Seq, SessionId};
use std::sync::Arc;

/// The extension state fact in which a history policy records ancestry.
#[derive(Debug)]
pub struct HistoryNamespace {
    pub ext: Name,
    pub state: Name,
}

#[derive(Debug)]
pub struct ValidatedPrefix {
    entries: Vec<Entry>,
    parent: ForkParent,
    namespace: HistoryNamespace,
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
            namespace: policy.namespace(),
            parent: ForkParent {
                session: source,
                seq,
            },
        })
    }

    pub fn into_history(self) -> (Vec<Entry>, ForkParent, HistoryNamespace) {
        (self.entries, self.parent, self.namespace)
    }
}
