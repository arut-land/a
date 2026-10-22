//! Named model context is recorded with its identity in the same sealed line.

use crate::{Body, CallId, Entry, Json, Outcome, Seq};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    pub namespace: String,
    pub key: Json,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publications(BTreeMap<u64, Vec<Publication>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnerKind {
    Agent,
    ModelOnly,
}

/// Context is decoded here so reducers need not understand engine authority.
#[derive(Serialize, Deserialize)]
pub struct SpawnImage {
    pub runner: RunnerKind,
    pub authority: Json,
    pub image: ContextImage,
}

/// Scope indices qualify local sequence numbers without nesting spawn envelopes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextImage {
    pub scopes: Vec<RunnerKind>,
    pub inputs: Vec<ContextInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextInput {
    pub scope: usize,
    pub seq: Seq,
    pub value: ContextValue,
}

/// Only projection facts can cross this boundary; no executable ledger is copied.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ContextValue {
    Inherited(usize),
    User(String),
    Queued(String),
    Taken(Seq),
    Assistant(String),
    Context {
        text: String,
        publication: Option<Publication>,
    },
    Child(Outcome),
    Reply(Json),
    ToolCall {
        id: String,
        tool: String,
    },
    ToolResult {
        call: CallId,
        outcome: Outcome,
    },
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
        let image: Self = json.parse()?;
        let mut owners = vec![false; image.image.scopes.len()];
        let mut positions = vec![0; owners.len()];
        for input in &image.image.inputs {
            let invalid = input.scope >= owners.len() || input.seq.get() <= positions[input.scope];
            if invalid {
                return Err(serde::de::Error::custom("invalid context position"));
            }
            positions[input.scope] = input.seq.get();
            if let ContextValue::Inherited(child) = input.value {
                if child <= input.scope || child >= owners.len() || owners[child] {
                    return Err(serde::de::Error::custom("invalid inherited scope"));
                }
                owners[child] = true;
            }
        }
        if owners.iter().skip(1).any(|owned| !owned) {
            return Err(serde::de::Error::custom("unowned context scope"));
        }
        Ok(image)
    }

    pub fn to_json(&self) -> Result<Json, serde_json::Error> {
        Json::encode(self)
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
                    .inputs
                    .iter()
                    .filter_map(|input| {
                        if let ContextValue::Context { publication, .. } = &input.value {
                            publication.clone()
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
