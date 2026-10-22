//! Derived identities retain log positions; payload eviction never changes replay.

use std::collections::HashMap;
use std::io;

use protocol::{Body, CallId, Decode, Entry, Seq, SessionId};

use crate::outbox::FrameLog;

#[derive(Default)]
pub(crate) struct Index {
    through: u64,
    fence: u64,
    sides: HashMap<ext::SideIdentity, CallId>,
    threads: HashMap<ext::ThreadKey, CallId>,
    results: HashMap<CallId, (Seq, bool)>,
    interrupt: u64,
    watches: Vec<(SessionId, Seq)>,
    fired: HashMap<Seq, Seq>,
    hot: Option<(Seq, Vec<u8>)>,
}

impl Index {
    pub(crate) fn update(&mut self, frames: &FrameLog, fence: u64) -> io::Result<()> {
        if self.fence != fence {
            *self = Self {
                fence,
                ..Self::default()
            };
        }
        for batch in frames.batches_after(self.through) {
            for entry in batch? {
                self.apply(&entry);
            }
        }
        Ok(())
    }

    pub(crate) fn apply(&mut self, entry: &Entry) {
        if entry.seq.get() <= self.through {
            return;
        }
        match &entry.body {
            Body::ModelCall {
                r#for: Some(_),
                request: Some(request),
                purpose: Some(purpose),
                ..
            } => {
                if let Ok(identity) = ext::SideIdentity::from_intent(request)
                    && identity.thread.purpose == *purpose
                {
                    self.threads
                        .entry(identity.thread.clone())
                        .or_insert(CallId(entry.seq));
                    self.sides.entry(identity).or_insert(CallId(entry.seq));
                }
            }
            Body::ModelResult { call, outcome, .. } | Body::ToolResult { call, outcome, .. } => {
                self.results
                    .entry(*call)
                    .or_insert((entry.seq, matches!(outcome, protocol::Outcome::Done { .. })));
            }
            Body::Interrupt { .. } => self.interrupt = entry.seq.get(),
            Body::Watch { child } => {
                if let Err(at) = self.watches.binary_search_by_key(&child.0, |(id, _)| id.0) {
                    self.watches.insert(at, (*child, entry.seq));
                }
            }
            Body::Fired { watch, .. } => {
                self.fired.entry(*watch).or_insert(entry.seq);
            }
            _ => {}
        }
        self.through = entry.seq.get();
    }

    pub(crate) fn thread(&self, key: &ext::ThreadKey) -> Option<CallId> {
        self.threads.get(key).copied()
    }

    pub(crate) fn side(&self, identity: &ext::SideIdentity) -> Option<CallId> {
        self.sides.get(identity).copied()
    }

    pub(crate) fn result(&self, call: CallId, durable: u64) -> Option<Seq> {
        self.results
            .get(&call)
            .map(|(seq, _)| *seq)
            .filter(|seq| seq.get() <= durable)
    }

    pub(crate) fn succeeded(&self, of: CallId) -> bool {
        of.0.get() > self.interrupt && self.results.get(&of).is_some_and(|(_, done)| *done)
    }

    pub(crate) fn fired(&self, child: SessionId) -> Option<Seq> {
        self.watches
            .binary_search_by_key(&child.0, |(id, _)| id.0)
            .ok()
            .and_then(|at| self.fired.get(&self.watches[at].1))
            .copied()
    }

    pub(crate) fn entry(&mut self, frames: &FrameLog, seq: Seq) -> io::Result<Entry> {
        const BUDGET: usize = 64 << 10;
        if let Some((at, bytes)) = &self.hot
            && *at == seq
        {
            return Entry::from_json(bytes).map_err(io::Error::other);
        }
        let entry = frames
            .batches_after(seq.get() - 1)
            .next()
            .ok_or_else(|| io::Error::other("indexed entry is missing"))??
            .into_iter()
            .next()
            .ok_or_else(|| io::Error::other("empty indexed batch"))?;
        let mut bytes = Vec::new();
        entry.encode(&mut bytes)?;
        if bytes.len() <= BUDGET {
            self.hot = Some((seq, bytes));
        }
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Header, Jsonl, LogStore};
    use protocol::{Envelope, EnvelopeId, Epoch, Json, Outcome, ToolChoice};

    fn history() -> (Vec<Entry>, ext::ModelTurn, SessionId, Envelope) {
        let child = SessionId::new();
        let side = ext::ModelTurn::new(
            ext::ThreadKey::new::<ext_title::Title>("index", ext::OwnerScope::Session),
            ext::TurnKey("initial".into()),
            ext::ModelRole::Fast,
            ext::SideContext::None,
            llm::Messages::default(),
        );
        let owner = CallId::new(1).unwrap();
        let envelope = Envelope {
            id: EnvelopeId {
                sender: SessionId::new(),
                call: owner,
                purpose: "delivery".into(),
            },
            destination: child,
            payload: "message".into(),
        };
        let mut bodies = vec![
            Body::ModelCall {
                model: "test".into(),
                attempt: 0,
                choice: ToolChoice::default(),
                r#for: Some(owner),
                purpose: Some(side.thread.purpose.clone()),
                request: Some(
                    protocol::publication::ThreadImage {
                        turn: side.intent(None).unwrap(),
                        authority: Json::null(),
                    }
                    .to_json()
                    .unwrap(),
                ),
            },
            Body::ModelResult {
                call: owner,
                r#for: Some(owner),
                outcome: Outcome::Done {
                    output: Json::encode(&"x".repeat(32768)).unwrap(),
                },
                failure: None,
            },
            Body::Watch { child },
            Body::Fired {
                watch: Seq::new(3).unwrap(),
                outcome: Outcome::Interrupted { output: None },
            },
            Body::Enqueue {
                envelope: envelope.clone(),
            },
            Body::Take {
                input: Seq::new(5).unwrap(),
            },
            Body::Outbox {
                envelope: envelope.clone(),
            },
            Body::Ack {
                envelope_id: envelope.id.clone(),
                refusal: None,
            },
        ];
        for n in 0..4 {
            bodies.push(Body::ModelResult {
                call: CallId::new(20 + n).unwrap(),
                r#for: Some(owner),
                outcome: Outcome::Done {
                    output: Json::encode(&"y".repeat(32768)).unwrap(),
                },
                failure: None,
            });
        }
        (
            bodies
                .into_iter()
                .enumerate()
                .map(|(at, body)| Entry::new(Seq::new(at as u64 + 1).unwrap(), Epoch(0), body))
                .collect(),
            side,
            child,
            envelope,
        )
    }

    #[test]
    fn deduplication_survives_index_eviction_and_restart() {
        let dir = std::env::temp_dir().join(format!("a-index-{}", SessionId::new()));
        let header = Header {
            id: SessionId::new(),
            workspace: "/w".into(),
            parent: None,
        };
        let (entries, side, child, envelope) = history();
        {
            let store = Jsonl::open(&dir).unwrap();
            store.import(&header, &entries).unwrap();
        }
        for _ in 0..2 {
            let store = Jsonl::open(&dir).unwrap();
            let mut ledger = protocol::ledger::Ledger::default();
            let loaded = store
                .load(header.id, &mut |_| false, &mut |e| ledger.apply(&e))
                .unwrap();
            let frames = FrameLog::new(loaded.lines, loaded.seq);
            let mut index = Index::default();
            index.update(&frames, 0).unwrap();
            let owner = CallId::new(1).unwrap();
            assert_eq!(index.side(&side.identity()), Some(owner));
            let result = index.result(owner, frames.seq()).unwrap();
            let before = index.entry(&frames, result).unwrap();
            for n in 9..=12 {
                index.entry(&frames, Seq::new(n).unwrap()).unwrap();
            }
            assert!(index.hot.as_ref().map_or(0, |(_, bytes)| bytes.len()) <= 64 << 10);
            assert!(index.hot.as_ref().is_none_or(|(seq, _)| *seq != result));
            assert_eq!(index.entry(&frames, result).unwrap(), before);
            assert_eq!(index.fired(child), Seq::new(4));
            ledger = Json::encode(&ledger).unwrap().parse().unwrap();
            ledger.apply(&Entry::new(
                Seq::new(13).unwrap(),
                Epoch(0),
                Body::Enqueue {
                    envelope: envelope.clone(),
                },
            ));
            assert!(ledger.received(&envelope.id));
            assert_eq!(ledger.inbox_len(), 0);
            assert_eq!(ledger.acknowledged(&envelope.id), Some(&None));
            assert_eq!(ledger.outbox(), []);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rebuilding_indexes_changes_no_outcome() {
        let (entries, side, child, _) = history();
        let store = crate::store::Memory::default();
        let header = Header {
            id: SessionId::new(),
            workspace: "/w".into(),
            parent: None,
        };
        store.import(&header, &entries).unwrap();
        let loaded = store.load(header.id, &mut |_| false, &mut |_| {}).unwrap();
        let frames = FrameLog::new(loaded.lines, loaded.seq);
        let mut incomplete = entries[0].clone();
        if let Body::ModelCall { purpose, .. } = &mut incomplete.body {
            *purpose = None;
        }
        let mut ignored = Index::default();
        ignored.apply(&incomplete);
        assert_eq!(ignored.side(&side.identity()), None);
        let mut live = Index::default();
        for entry in &entries {
            live.apply(entry);
        }
        let mut rebuilt = Index::default();
        rebuilt.update(&frames, 0).unwrap();
        let owner = CallId::new(1).unwrap();
        assert_eq!(live.side(&side.identity()), rebuilt.side(&side.identity()));
        for seq in [
            live.result(owner, frames.seq()).unwrap(),
            live.fired(child).unwrap(),
        ] {
            assert_eq!(
                live.entry(&frames, seq).unwrap(),
                entries[usize::try_from(seq.get()).unwrap() - 1]
            );
            assert_eq!(
                rebuilt.entry(&frames, seq).unwrap(),
                live.entry(&frames, seq).unwrap()
            );
        }
        let pending = Entry::new(
            Seq::new(13).unwrap(),
            Epoch(0),
            Body::ModelResult {
                call: CallId::new(99).unwrap(),
                r#for: Some(owner),
                outcome: Outcome::Interrupted { output: None },
                failure: None,
            },
        );
        live.apply(&pending);
        assert_eq!(live.result(CallId::new(99).unwrap(), frames.seq()), None);
        assert_eq!(live.result(CallId::new(99).unwrap(), 13), Some(pending.seq));
        live.update(&frames, 1).unwrap();
        assert_eq!(live.result(CallId::new(99).unwrap(), 13), None);
        assert_eq!(
            live.result(owner, frames.seq()),
            rebuilt.result(owner, frames.seq())
        );
    }
}
