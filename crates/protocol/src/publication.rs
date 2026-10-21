//! Named model context is recorded with its identity in the same sealed line.

use crate::{Body, Entry, Json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publication {
    pub namespace: String,
    pub key: Json,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Publications(BTreeMap<u64, Publication>);

impl Publications {
    pub fn apply(&mut self, entry: &Entry) {
        if let Body::Edit { target, upto, .. } = &entry.body {
            let end = upto.unwrap_or(*target).get();
            self.0.retain(|seq, _| *seq < target.get() || *seq > end);
        }
        if let Body::Context {
            publication: Some(publication),
            ..
        } = &entry.body
        {
            self.0.insert(entry.seq.get(), publication.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, namespace: &str, key: &Json) -> bool {
        self.0
            .values()
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
