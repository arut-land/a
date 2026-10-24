//! Named model context is recorded with its identity in the same sealed line.

use crate::{Body, CallId, Entry, Envelope, Json, Mode, Outcome, Segments, Seq};
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
    pub image: ContextImage,
}

/// Scope indices qualify local sequence numbers without nesting spawn envelopes.
pub type ContextImage = Vec<ContextLayer>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextLayer(pub RunnerKind, pub Vec<ContextInput>);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextInput(pub Seq, pub ContextValue);

/// Only projection facts can cross this boundary; no executable ledger is copied.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ContextValue {
    Inherited(usize),
    User(String),
    /// Input waiting to be taken up, as it was given: how a model reads
    /// it depends on who takes it.
    Queued(Segments, Mode),
    /// A message another session sent, waiting to be taken up.
    Inbox(Envelope),
    Taken(Seq),
    /// Taken into the result of a call, read with it.
    Into(Seq, CallId),
    Withdrawn(Seq),
    Assistant(String),
    Context(String, Option<Publication>),
    /// A queued child result.
    Child(Outcome),
    /// The call's result is a placeholder: it went on in the background.
    Detached(CallId),
    /// A queued outcome of a call that went on in the background.
    Background(CallId, Outcome),
    /// A queued end of a process group call `call` left to the session.
    Job(CallId, String),
    Reply(Json),
    ToolCall(String, String),
    /// A result, and what it holds outside the log: how its references
    /// are named, by whoever projects it.
    ToolResult(CallId, Outcome, Vec<crate::Held>),
}

/// Execution identity belongs to the owner, never to copied audit history.
#[derive(Clone)]
pub struct ThreadImage {
    pub turn: Json,
    pub authority: Json,
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
        let (runner, authority, image): (RunnerKind, Json, ContextImage) = json.parse()?;
        let image = Self {
            runner,
            authority,
            image,
        };
        let mut owners = vec![false; image.image.len()];
        for (scope, ContextLayer(_, inputs)) in image.image.iter().enumerate() {
            let mut position = 0;
            for ContextInput(seq, value) in inputs {
                if seq.get() <= position {
                    return Err(serde::de::Error::custom("invalid context position"));
                }
                position = seq.get();
                if let ContextValue::Inherited(child) = *value {
                    if child <= scope || child >= owners.len() || owners[child] {
                        return Err(serde::de::Error::custom("invalid inherited scope"));
                    }
                    owners[child] = true;
                }
            }
        }
        if owners.iter().skip(1).any(|owned| !owned) {
            return Err(serde::de::Error::custom("unowned context scope"));
        }
        Ok(image)
    }

    pub fn to_json(&self) -> Result<Json, serde_json::Error> {
        Json::encode(&(self.runner, &self.authority, &self.image))
    }
}

impl Publications {
    pub fn apply(&mut self, entry: &Entry) {
        if let Body::Spawned { intent } = &entry.body
            && let Ok(image) = SpawnImage::from_json(intent)
        {
            self.0.insert(
                entry.seq.get(),
                image
                    .image
                    .into_iter()
                    .flat_map(|ContextLayer(_, inputs)| inputs)
                    .filter_map(|ContextInput(_, value)| {
                        if let ContextValue::Context(_, publication) = value {
                            publication
                        } else {
                            None
                        }
                    })
                    .collect(),
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

#[cfg(test)]
#[path = "publication/context_tests.rs"]
mod context_tests;
