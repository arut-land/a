//! Log-derived public controls and seek positions, shared by attached clients.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use protocol::history::{Head, Page, Request};
use protocol::{Epoch, HostPath, SessionId};

use crate::lock;
use crate::outbox::FrameLog;

pub(crate) struct Owner {
    pub session: SessionId,
    pub parent: Option<SessionId>,
    pub workspace: HostPath,
    pub generation: Epoch,
}

pub(crate) struct History {
    session: SessionId,
    workspace: HostPath,
    parent: Option<SessionId>,
    generation: AtomicU32,
    index: Mutex<Option<Arc<Mutex<Indexed>>>>,
}

struct Indexed {
    session: SessionId,
    index: protocol::view::Index,
    conversation: protocol::history::Conversation,
    clocks: BTreeMap<SessionId, (u64, u64)>,
}

impl Indexed {
    fn apply(&mut self, entry: &protocol::Entry) {
        use protocol::{Body, ledger::OutcomeKind};
        self.index.apply(entry);
        let meta = &mut self.conversation;
        match &entry.body {
            Body::Spawned { intent } | Body::Link { intent, .. } => {
                if let Ok(spawn) = crate::driver::spawn::SpawnIntent::from_json(intent) {
                    let agent = spawn
                        .request
                        .runner
                        .agent()
                        .unwrap_or("model_only")
                        .to_owned();
                    let title = spawn
                        .request
                        .prompt
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .chars()
                        .take(96)
                        .collect();
                    let admitted = u64::try_from(
                        spawn
                            .admitted_at
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                    )
                    .unwrap_or(u64::MAX);
                    if let Body::Link { child, .. } = &entry.body {
                        meta.children.push(protocol::history::Child {
                            session: *child,
                            agent,
                            title,
                            admitted_unix_ms: admitted,
                        });
                    } else {
                        meta.agent = Some(agent);
                        meta.title = Some(title);
                        meta.admitted_unix_ms = Some(admitted);
                    }
                }
            }
            Body::Input { .. } | Body::Take { into: None, .. } => meta.outcome = None,
            Body::Interrupt { .. } => meta.outcome = Some(OutcomeKind::Interrupted),
            Body::ModelResult {
                outcome,
                r#for: None,
                ..
            } => {
                if meta.outcome != Some(OutcomeKind::Interrupted) {
                    meta.outcome = Some(OutcomeKind::of(outcome));
                }
            }
            Body::ToolResult { outcome, .. } => {
                if meta.outcome != Some(OutcomeKind::Interrupted) {
                    meta.outcome = Some(OutcomeKind::of(outcome));
                }
            }
            Body::Assistant { .. } => meta.outcome = Some(OutcomeKind::Done),
            _ => {}
        }
        if let Some(fact) = entry
            .execution
            .as_ref()
            .and_then(|json| protocol::CallFact::from_json(json).ok())
            && !fact.side
            && fact.id.executing_session == self.session
        {
            if meta.agent.is_none() {
                meta.agent = Some(fact.agent);
            }
            if let Some(elapsed) = fact.elapsed_ns {
                let range = self
                    .clocks
                    .entry(fact.clock_epoch)
                    .or_insert((fact.start_ns, fact.start_ns));
                range.0 = range.0.min(fact.start_ns);
                range.1 = range.1.max(fact.start_ns.saturating_add(elapsed));
                meta.elapsed_ns = Some(
                    self.clocks
                        .values()
                        .fold(0_u64, |sum, (start, end)| sum.saturating_add(end - start)),
                );
            }
        }
    }
}

impl History {
    pub(crate) fn new(owner: Owner) -> Self {
        Self {
            session: owner.session,
            parent: owner.parent,
            workspace: owner.workspace,
            generation: AtomicU32::new(owner.generation.0),
            index: Mutex::default(),
        }
    }

    pub(crate) fn reset(&self, generation: Epoch) {
        self.generation.store(generation.0, Ordering::Release);
        *lock(&self.index) = None;
    }

    fn index(&self) -> Arc<Mutex<Indexed>> {
        Arc::clone(lock(&self.index).get_or_insert_with(|| {
            Arc::new(Mutex::new(Indexed {
                session: self.session,
                index: protocol::view::Index::new(self.session, self.workspace.clone()),
                clocks: BTreeMap::new(),
                conversation: protocol::history::Conversation {
                    parent: self.parent,
                    ..Default::default()
                },
            }))
        }))
    }

    pub(crate) fn is_indexed(&self) -> bool {
        lock(&self.index).is_some()
    }

    /// Reuse the writer's decoded facts without making publication wait for a reader.
    pub(crate) fn observe(&self, entry: &protocol::Entry) {
        let Some(index) = lock(&self.index).as_ref().map(Arc::clone) else {
            return;
        };
        let mut index = match index.try_lock() {
            Ok(index) => index,
            Err(TryLockError::WouldBlock) => return,
            Err(TryLockError::Poisoned(_)) => panic!("history index lock poisoned"),
        };
        if index.index.through() + 1 == entry.seq.get() {
            index.apply(entry);
        }
    }

    #[cfg(test)]
    pub(crate) fn head(&self, frames: &FrameLog, since: u64) -> io::Result<Head> {
        self.head_for(frames, since, &[])
    }

    pub(crate) fn head_for(
        &self,
        frames: &FrameLog,
        since: u64,
        purposes: &[String],
    ) -> io::Result<Head> {
        let index = self.index();
        let mut index = lock(&index);
        for batch in frames.batches_after(index.index.through()) {
            for entry in batch? {
                index.apply(&entry);
            }
        }
        let mut head = index.index.head_for(
            Epoch(self.generation.load(Ordering::Acquire)),
            since,
            purposes,
        );
        head.conversation = index.conversation.clone();
        Ok(head)
    }

    pub(crate) fn page(&self, frames: &FrameLog, request: &Request) -> io::Result<Page> {
        if request.cut.session != self.session
            || request.cut.generation.0 != self.generation.load(Ordering::Acquire)
            || request.cut.seq > frames.seq()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "history cut is no longer current",
            ));
        }
        let index = self.index();
        let mut index = lock(&index);
        for batch in frames.batches_after(index.index.through()) {
            for entry in batch? {
                index.apply(&entry);
            }
        }
        index.index.page(request, |seq| frames.entry(seq.get()))
    }
}

/// Chunking bounds delivery, including a single item larger than a frame.
pub(crate) async fn answer(
    daemon: Arc<crate::Daemon>,
    outbox: crate::outbox::Outbox,
    id: u64,
    request: Request,
) -> Result<protocol::Json, crate::query::Ended> {
    let failed = |error: io::Error| crate::query::Ended::Failed(error.to_string());
    let json = {
        let slot = daemon
            .slot(request.cut.session)
            .ok_or_else(|| crate::query::Ended::Failed("no such history session".into()))?;
        let live = daemon.live(&slot).await.map_err(failed)?;
        let frames = Arc::clone(&live.frames);
        rt::try_unblock_with(
            &*daemon.shared.rt,
            rt::BlockingClass::Cpu,
            rt::STORAGE_BYTES,
            move || {
                let page = frames.history.page(&frames, &request)?;
                serde_json::to_string(&page).map_err(io::Error::other)
            },
        )
        .await
        .map_err(failed)?
        .map_err(failed)?
    };
    let mut at = 0;
    while at < json.len() {
        let mut end = (at + (64 << 10)).min(json.len());
        while !json.is_char_boundary(end) {
            end -= 1;
        }
        let frame = protocol::Frame::new(&protocol::DaemonMsg::HistoryPart {
            id,
            offset: at as u64,
            text: json[at..end].into(),
            done: end == json.len(),
        })
        .map_err(failed)?;
        outbox
            .send_paced(frame)
            .await
            .map_err(|_| crate::query::Ended::Failed("the client hung up".into()))?;
        at = end;
    }
    protocol::Json::encode(&()).map_err(|error| crate::query::Ended::Failed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Header, Jsonl, LogStore};
    use protocol::Body;

    #[test]
    fn execution_spans_include_gaps_without_combining_clock_epochs() {
        let session = SessionId::new();
        let clock = SessionId::new();
        let mut indexed = Indexed {
            session,
            index: protocol::view::Index::new(session, "/test".into()),
            conversation: protocol::history::Conversation::default(),
            clocks: BTreeMap::new(),
        };
        for (at, (epoch, start, elapsed)) in
            [(clock, 10, 1), (clock, 15, 1), (SessionId::new(), 2, 2)]
                .into_iter()
                .enumerate()
        {
            let call = protocol::CallId::new(at as u64 * 2 + 1).unwrap();
            indexed.apply(&protocol::Entry::new(
                protocol::Seq::new(call.get()).unwrap(),
                Epoch(1),
                Body::ModelCall {
                    model: "fixture".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: None,
                    purpose: None,
                    request: None,
                },
            ));
            let mut result = protocol::Entry::new(
                protocol::Seq::new(call.get() + 1).unwrap(),
                Epoch(1),
                Body::ModelResult {
                    call,
                    r#for: None,
                    outcome: protocol::Outcome::Done {
                        output: protocol::Json::null(),
                    },
                    failure: None,
                },
            );
            result.execution = Some(
                protocol::CallFact {
                    id: protocol::AttemptId {
                        executing_session: session,
                        call,
                        attempt: 0,
                    },
                    model: "fixture".into(),
                    role: "session".into(),
                    runner: "agent".into(),
                    agent: "echo".into(),
                    side: false,
                    unix_ms: 0,
                    clock_epoch: epoch,
                    start_ns: start * 1_000_000_000,
                    elapsed_ns: Some(elapsed * 1_000_000_000),
                    usage: None,
                    charge: None,
                }
                .to_json()
                .unwrap(),
            );
            indexed.apply(&result);
        }
        assert_eq!(indexed.conversation.elapsed_ns, Some(8_000_000_000));
    }

    #[test]
    fn committed_facts_update_a_hot_index_without_rereading_or_waiting_for_readers() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let store = Jsonl::open(root.path().join("sessions")).unwrap();
        let mut log = store
            .create(&Header {
                id: session,
                workspace: root.path().into(),
                parent: None,
            })
            .unwrap();
        let frames = FrameLog::with_owner(
            Arc::clone(&log.lines),
            0,
            Owner {
                session,
                parent: None,
                workspace: "/test".into(),
                generation: Epoch(1),
            },
        );
        assert_eq!(frames.history.head(&frames, 0).unwrap().count, 0);
        let first = log
            .append(Body::Assistant {
                text: "first".into(),
            })
            .unwrap();
        frames.push(first.seq.get());
        frames.history.observe(&first);
        let file = root
            .path()
            .join("sessions")
            .join(session.to_string())
            .join("log.jsonl");
        let moved = file.with_extension("held");
        std::fs::rename(&file, &moved).unwrap();
        let head = frames.history.head(&frames, 0).unwrap();
        assert_eq!(head.count, 1);
        std::fs::rename(&moved, &file).unwrap();
        let index = frames.history.index();
        let reader = lock(&index);
        let second = log
            .append(Body::Assistant {
                text: "second".into(),
            })
            .unwrap();
        frames.push(second.seq.get());
        frames.history.observe(&second);
        assert_eq!(reader.index.through(), first.seq.get());
        drop(reader);
        let head = frames.history.head(&frames, first.seq.get()).unwrap();
        assert_eq!((head.cut.seq, head.count), (second.seq.get(), 2));
    }
}
