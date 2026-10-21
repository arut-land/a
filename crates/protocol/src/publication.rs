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
