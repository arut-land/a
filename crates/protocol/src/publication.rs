//! Named model context is recorded with its identity in the same sealed line.

use crate::{Body, CallId, Entry, Json, Mode, Segments, Seq, SessionId};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    pub namespace: String,
    pub key: Json,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publications(BTreeMap<u64, Vec<Publication>>);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnerKind {
    #[default]
    Agent,
    ModelOnly,
}

/// Context is decoded here so reducers need not understand engine authority.
pub struct SpawnImage {
    pub runner: RunnerKind,
    pub authority: Json,
    pub image: ContextCut,
}

/// The immutable prefix of an ordinary conversation log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LogCut {
    pub session: SessionId,
    pub through: Seq,
}

/// Context authority references source facts, never a second transcript representation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCut {
    pub runner: RunnerKind,
    pub source: Option<LogCut>,
    pub presentation: Option<LogCut>,
    pub publications: Publications,
}

#[derive(Clone, Debug, Default)]
pub struct ContextLayer(pub RunnerKind, pub Vec<ContextInput>);

#[derive(Clone, Debug)]
pub struct ContextInput(pub Seq, pub ContextValue);

/// Transient projection inputs; durable context contains only ordinary log references.
#[derive(Clone, Debug)]
pub enum ContextValue {
    /// An unprojected position can still be the anchor of a later edit.
    Position,
    Inherited(usize),
    User(String),
    /// Input waiting to be taken up, as it was given: how a model reads
    /// it depends on who takes it.
    Queued(Segments, Mode),
    /// A message another session sent, waiting to be taken up.
    Inbox(crate::EnvelopeId, String),
    Taken(Seq),
    /// Taken into the result of a call, read with it.
    Into(Seq, CallId),
    Withdrawn(Seq),
    Assistant(String),
    Context(String, Option<Publication>),
    /// A queued child result.
    Child(crate::ToolOutcome),
    /// The call's result is a placeholder: it went on in the background.
    Detached(CallId),
    /// A queued outcome of a call that went on in the background, and
    /// what it holds outside the log.
    Background(CallId, crate::ToolOutcome, Vec<crate::Held>),
    /// A queued end of a process group call `call` left to the session,
    /// and what it holds outside the log: the group's output, sealed.
    Job(CallId, String, Vec<crate::Held>),
    Reply(Json),
    ToolCall(String, String),
    /// A result, and what it holds outside the log: how its references
    /// are named, by whoever projects it.
    ToolResult(CallId, crate::ToolOutcome, Vec<crate::Held>),
}

impl ContextValue {
    /// Payload references in a projected log fact.
    pub fn blobs(&self) -> Vec<crate::Held> {
        let mut found = Vec::new();
        match self {
            Self::Queued(segments, _) => crate::segment_blobs(segments, &mut found),
            Self::Child(outcome) => crate::outcome_blob(outcome, &mut found),
            Self::Background(_, outcome, held) | Self::ToolResult(_, outcome, held) => {
                found.extend(held.iter().cloned());
                crate::outcome_blob(outcome, &mut found);
            }
            Self::Job(_, _, held) => found.extend(held.iter().cloned()),
            Self::Position
            | Self::Inherited(_)
            | Self::User(_)
            | Self::Inbox(..)
            | Self::Taken(_)
            | Self::Into(..)
            | Self::Withdrawn(_)
            | Self::Assistant(_)
            | Self::Context(..)
            | Self::Detached(_)
            | Self::Reply(_)
            | Self::ToolCall(..) => {}
        }
        found
    }
}

/// Execution identity belongs to the owner, never to copied audit history.
#[derive(Clone)]
pub struct ThreadImage {
    pub turn: Json,
    pub authority: Json,
}

/// One side input names its frozen owner cut; thread authority is admitted separately.
pub struct ThreadInput {
    pub turn: Json,
    pub context: ContextCut,
}

impl ThreadInput {
    pub fn from_json(json: &Json) -> serde_json::Result<Self> {
        let (turn, context) = json.parse()?;
        Ok(Self { turn, context })
    }

    pub fn to_json(&self) -> serde_json::Result<Json> {
        Json::encode(&(&self.turn, &self.context))
    }
}

impl ThreadImage {
    pub fn from_json(json: &Json) -> serde_json::Result<Self> {
        let (turn, authority) = json.parse()?;
        Ok(Self { turn, authority })
    }
    pub fn to_json(&self) -> serde_json::Result<Json> {
        Json::encode(&(&self.turn, &self.authority))
    }
}

impl SpawnImage {
    pub fn from_json(json: &Json) -> Result<Self, serde_json::Error> {
        let (runner, authority, image): (RunnerKind, Json, ContextCut) = json.parse()?;
        let image = Self {
            runner,
            authority,
            image,
        };
        Ok(image)
    }

    pub fn to_json(&self) -> Result<Json, serde_json::Error> {
        Json::encode(&(self.runner, &self.authority, &self.image))
    }
}

impl Publications {
    pub fn apply(&mut self, entry: &Entry) {
        // Public folds need identities, not execution or source authority.
        #[derive(Deserialize)]
        struct InheritedPublications {
            publications: Publications,
        }
        if let Body::Spawned { intent } = &entry.body
            && let Ok((_, _, image)) =
                intent.parse::<(IgnoredAny, IgnoredAny, InheritedPublications)>()
        {
            self.0.insert(
                entry.seq.get(),
                image.publications.0.into_values().flatten().collect(),
            );
        }
        if let Body::Edit { target, upto, .. } = &entry.body {
            let end = upto.unwrap_or(*target).get();
            self.0.retain(|seq, _| *seq < target.get() || *seq > end);
        }
        if let Body::Context {
            publication: Some(publication),
            ..
        } = &entry.body
        {
            self.0.insert(entry.seq.get(), vec![publication.clone()]);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.values().all(Vec::is_empty)
    }

    pub fn contains(&self, namespace: &str, key: &Json) -> bool {
        self.0
            .values()
            .flatten()
            .any(|publication| publication.namespace == namespace && publication.key == *key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Epoch, Seq};

    #[test]
    fn checkpointed_publications_survive_only_while_their_text_is_effective() {
        let key = Json::encode("review").unwrap();
        let entry = |seq, body| Entry::new(Seq::new(seq).unwrap(), Epoch::default(), body);
        let mut ledger = crate::ledger::Ledger::default();
        for seq in [1, 3] {
            ledger.apply(&entry(
                seq,
                Body::Context {
                    text: "instructions".into(),
                    publication: Some(Publication {
                        namespace: "index".into(),
                        key: key.clone(),
                    }),
                },
            ));
        }
        let encoded = Json::encode(&ledger).unwrap();
        let mut restored: crate::ledger::Ledger = encoded.parse().unwrap();
        for (target, expected) in [(1, true), (3, false)] {
            restored.apply(&entry(
                target + 4,
                Body::Edit {
                    target: Seq::new(target).unwrap(),
                    upto: None,
                    replace: None,
                },
            ));
            assert_eq!(restored.publications().contains("index", &key), expected);
        }
    }
}
