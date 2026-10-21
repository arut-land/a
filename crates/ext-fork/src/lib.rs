//! Conversation-only branching. A prefix must be at rest: inherited calls,
//! approvals and agent actions must never run twice in different sessions.

use std::sync::Arc;

use ext::host::{Binding, HistoryPolicy};
use ext::{Configured, Extension, Refused, Registrar, Table};
use protocol::{Body, Entry, Json, Seq, SessionId};
use serde::{Deserialize, Serialize};

pub struct Fork;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
    pub session: SessionId,
    pub seq: Seq,
}

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
    fn prepare(
        &self,
        binding: Arc<dyn Binding>,
        entries: &[Entry],
        source: SessionId,
        seq: Seq,
    ) -> Result<Body, Refused> {
        let mut core = kernel::Core::new(binding);
        for entry in entries {
            core.load(entry);
        }
        // A terminal agent stop is pure and has no separate log entry.
        // Anything recovery would write or start makes this an unfinished prefix.
        let effects = core.resumed(&kernel::Load::default());
        if !core.at_rest()
            || effects
                .iter()
                .any(|effect| !matches!(effect, kernel::Effect::MarkBusy(false)))
        {
            return Err(Refused("fork requires a rest point: unfinished calls, approvals and queued actions cannot be copied".into()));
        }
        if !core.ledger().background().is_empty() || core.ledger().armed().next().is_some() {
            return Err(Refused(
                "fork requires a prefix without owned background processes or child watches".into(),
            ));
        }
        Ok(Body::Ext {
            ext: "fork".into(),
            state: "parent".into(),
            data: Json::encode(&Parent {
                session: source,
                seq,
            })
            .map_err(|e| Refused(e.to_string()))?,
        })
    }
}
