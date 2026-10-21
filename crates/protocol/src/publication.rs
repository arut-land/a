//! Named model context is recorded with its identity in the same sealed line.

use crate::{Body, Decode, Entry, Json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    pub namespace: String,
    pub key: Json,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publications(BTreeMap<u64, Vec<Publication>>);

/// Context is decoded here so reducers need not understand engine authority.
#[derive(Serialize, Deserialize)]
pub struct SpawnImage {
    pub authority: Json,
    pub image: Vec<Json>,
}

impl SpawnImage {
    pub fn from_json(json: &Json) -> Result<Self, serde_json::Error> {
        json.parse()
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
            let mut inherited = Self::default();
            for encoded in image.image {
                if let Ok(source) = Entry::from_json(encoded.raw().as_bytes()) {
                    inherited.apply(&source);
                }
            }
            self.0.insert(
                entry.seq.get(),
                inherited.0.into_values().flatten().collect(),
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
        self.0.is_empty()
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
