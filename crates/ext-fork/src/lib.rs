//! Conversation-only branching. A prefix must be at rest: inherited calls,
//! approvals and agent actions must never run twice in different sessions.

use std::sync::Arc;

use ext::host::{Binding, HistoryNamespace, HistoryPolicy};
use ext::{Configured, Extension, Refused, Registrar, Table};
use protocol::{Entry, ForkError};

pub struct Fork;

impl Extension for Fork {
    const NAME: &'static str = "fork";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.provide::<dyn HistoryPolicy>(Arc::new(Self));
    }
}

impl Configured for Fork {
    fn configure(_: &impl Table) -> Result<Self, Refused> {
        Ok(Self)
    }
}

impl HistoryPolicy for Fork {
    fn namespace(&self) -> HistoryNamespace {
        HistoryNamespace {
            ext: Self::NAME.into(),
            state: "parent".into(),
        }
    }

    fn validate(&self, binding: Arc<dyn Binding>, entries: &[Entry]) -> Result<(), ForkError> {
        let mut core = kernel::Core::new(binding);
        for entry in entries {
            core.load(entry);
        }
        // A terminal agent stop is pure and has no separate log entry.
        // Anything recovery would write or start makes this an unfinished prefix.
        let effects = core.resumed(&kernel::Load::default());
        if !core.at_rest()
            || !effects.appends.is_empty()
            || !effects.admitted.is_empty()
            || effects
                .immediate
                .iter()
                .any(|effect| !matches!(effect, kernel::Work::MarkBusy(false)))
        {
            return Err(ForkError::Unfinished);
        }
        if !core.ledger().background().is_empty() || core.ledger().armed().next().is_some() {
            return Err(ForkError::OwnedResources);
        }
        Ok(())
    }
}
