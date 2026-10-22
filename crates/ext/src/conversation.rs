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

pub use llm::ContextPolicy;

pub trait ModelOnlyKind {
    const CONTEXT: ContextPolicy;
}

impl ModelOnlyKind for crate::ModelOnly {
    const CONTEXT: ContextPolicy = ContextPolicy {
        exchanges: 8,
        bytes: 65536,
    };
}

#[derive(Clone, Debug)]
pub enum Runner {
    Agent {
        agent: String,
        role: ModelRole,
    },
    ModelOnly {
        role: ModelRole,
        context: ContextPolicy,
    },
}

impl Serialize for Runner {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        (self.agent(), self.role(), self.context_policy()).serialize(s)
    }
}
impl<'de> Deserialize<'de> for Runner {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let json = protocol::Json::deserialize(d)?;
        Self::from_json(&json).map_err(serde::de::Error::custom)
    }
}
impl Runner {
    fn from_json(json: &protocol::Json) -> serde_json::Result<Self> {
        let (agent, role, context): (Option<String>, ModelRole, Option<ContextPolicy>) =
            json.parse()?;
        Ok(match agent {
            Some(agent) => Self::Agent { agent, role },
            None => Self::ModelOnly {
                role,
                context: context.ok_or_else(|| {
                    <serde_json::Error as serde::de::Error>::custom(
                        "missing model-only context policy",
                    )
                })?,
            },
        })
    }
}

impl Runner {
    pub fn context_policy(&self) -> Option<ContextPolicy> {
        match self {
            Self::Agent { .. } => None,
            Self::ModelOnly { context, .. } => Some(*context),
        }
    }
    pub fn agent(&self) -> Option<&str> {
        match self {
            Self::Agent { agent, .. } => Some(agent),
            Self::ModelOnly { .. } => None,
        }
    }
    pub fn role(&self) -> &ModelRole {
        match self {
            Self::Agent { role, .. } | Self::ModelOnly { role, .. } => role,
        }
    }
}

#[derive(Clone, Debug)]
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
    pub fn to_json(&self) -> serde_json::Result<protocol::Json> {
        protocol::Json::encode(&(
            &self.owner,
            &self.purpose,
            &self.prompt,
            &self.context,
            &self.runner,
            self.depth,
            self.budget,
        ))
    }

    pub fn from_json(json: &protocol::Json) -> serde_json::Result<Self> {
        let (owner, purpose, prompt, context, runner, depth, budget) = json.parse()?;
        Ok(Self {
            owner,
            purpose,
            prompt,
            context,
            runner,
            depth,
            budget,
        })
    }

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

impl From<&crate::ModelTurn> for Spawn {
    fn from(turn: &crate::ModelTurn) -> Self {
        Self {
            owner: turn.thread.extension.clone(),
            purpose: turn.thread.purpose.clone(),
            prompt: String::new(),
            context: ContextSource::Fresh,
            runner: Runner::ModelOnly {
                role: turn.role.clone(),
                context: turn.context_policy(),
            },
            depth: DepthBudget::new(u16::MAX),
            budget: turn.budget,
        }
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
    fn context_bounds_keep_instructions_and_whole_exchanges() {
        let current: llm::Messages = [llm::Message::user("current instruction")]
            .into_iter()
            .collect();
        let history: llm::Messages = [
            llm::Message::system("thread system"),
            llm::Message::user("old instruction"),
            llm::Message::assistant("old reply"),
            llm::Message::user("recent instruction"),
            llm::Message::assistant("recent reply"),
        ]
        .into_iter()
        .chain(current.iter().cloned())
        .collect();
        let owner: llm::Messages = [
            llm::Message::system("owner system"),
            llm::Message::user("x".repeat(4096)),
        ]
        .into_iter()
        .collect();
        let policy = ContextPolicy {
            exchanges: 1,
            bytes: 512,
        };
        let assembled = policy.assemble(&history, &owner, &current).unwrap();
        assert_eq!(
            assembled,
            [
                llm::Message::user("recent instruction"),
                llm::Message::assistant("recent reply"),
                llm::Message::user("current instruction")
            ]
            .into_iter()
            .collect()
        );
        assert!(protocol::Json::encode(&assembled).unwrap().raw().len() <= 512);
        assert!(
            ContextPolicy {
                exchanges: 1,
                bytes: 1
            }
            .assemble(&history, &owner, &current)
            .is_err()
        );
    }

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
