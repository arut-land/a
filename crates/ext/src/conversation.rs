//! Typed requests for owned conversations. Execution semantics belong to
//! the request type, so a detached handle cannot accidentally be awaited.

use crate::{Budget, Extension, ModelRole};
use protocol::{Seq, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[must_use]
pub struct DepthBudget(u16);

impl DepthBudget {
    pub const fn new(edges: u16) -> Self {
        Self(edges)
    }
    pub const fn remaining(self) -> u16 {
        self.0
    }
    pub const fn descend(self) -> Result<Self, SpawnRefusal> {
        match self.0.checked_sub(1) {
            Some(edges) => Ok(Self(edges)),
            None => Err(SpawnRefusal::DepthExhausted),
        }
    }
    pub const fn intersect(self, other: Self) -> Self {
        Self(if self.0 < other.0 { self.0 } else { other.0 })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ContextSource {
    Fresh,
    Prefix { session: SessionId, seq: Seq },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Runner {
    pub agent: String,
    pub role: ModelRole,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Spawn {
    owner: String,
    pub purpose: String,
    pub prompt: String,
    pub context: ContextSource,
    pub runner: Runner,
    pub depth: DepthBudget,
    pub budget: Budget,
}

impl Spawn {
    pub fn new<E: Extension>(
        purpose: &str,
        prompt: String,
        runner: Runner,
        depth: DepthBudget,
    ) -> Self {
        Self {
            owner: E::NAME.into(),
            purpose: purpose.into(),
            prompt,
            context: ContextSource::Fresh,
            runner,
            depth,
            budget: Budget::default(),
        }
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn awaited(self) -> Awaited<Self> {
        Awaited(self)
    }
    pub fn detached(self) -> Detached<Self> {
        Detached(self)
    }
}

#[derive(Clone, Debug)]
pub struct Awaited<T>(pub T);
/// Detached requests cannot be passed to the awaited service.
/// ```compile_fail
/// fn wrong(sessions: &dyn ext::Sessions, cx: &ext::Cx, spawn: ext::Spawn) {
///     let _ = sessions.spawn(cx, spawn.detached());
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Detached<T>(pub T);

/// A durable address has no future or polling semantics.
/// ```compile_fail
/// async fn wrong(handle: ext::ConversationHandle) { handle.await; }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationHandle {
    pub session: SessionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnRefusal {
    DepthExhausted,
    Unavailable(String),
    Interrupted,
}

/// A sender chooses an extension-local purpose; the engine supplies its call id.
#[derive(Clone, Debug)]
pub struct SendMessage {
    pub(crate) owner: &'static str,
    pub purpose: String,
    pub destination: ConversationHandle,
    pub payload: String,
}
impl SendMessage {
    pub fn new<E: Extension>(
        purpose: &str,
        destination: ConversationHandle,
        payload: String,
    ) -> Self {
        Self {
            owner: E::NAME,
            purpose: purpose.into(),
            destination,
            payload,
        }
    }
    pub fn envelope(&self, sender: SessionId, call: protocol::CallId) -> protocol::Envelope {
        protocol::Envelope {
            id: protocol::EnvelopeId {
                sender,
                call,
                purpose: format!("{}:{}", self.owner, self.purpose),
            },
            destination: self.destination.session,
            payload: self.payload.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descending_and_intersection_never_increase_allowance() {
        for parent in 0..32 {
            for requested in 0..32 {
                let budget = DepthBudget::new(parent).intersect(DepthBudget::new(requested));
                match budget.descend() {
                    Ok(child) => {
                        assert!(child.remaining() < parent);
                        assert!(child.remaining() < requested);
                    }
                    Err(error) => {
                        assert_eq!(error, SpawnRefusal::DepthExhausted);
                        assert!(parent == 0 || requested == 0);
                    }
                }
            }
        }
    }
    #[test]
    fn persisted_depth_cannot_replenish_itself() {
        let depth = DepthBudget::new(2).descend().unwrap();
        let restored: DepthBudget = crate::Json::encode(&depth).unwrap().parse().unwrap();
        assert_eq!(
            restored.descend().unwrap().descend(),
            Err(SpawnRefusal::DepthExhausted)
        );
    }
}
