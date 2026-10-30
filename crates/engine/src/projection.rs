//! Speculative model context and durable client views have separate prefixes.

#[cfg(test)]
use protocol::view::SessionView;

use super::{
    Activity, Actor, Admission, Admitted, Arc, Body, CallId, Cpu, DaemonMsg, Entry, Frame,
    Ordering, Outbox, Transcript, io, lock,
};

impl super::Shared {
    pub(super) fn presentation_registry(
        &self,
        workspace: &std::path::Path,
        cut: &protocol::publication::ContextImage,
        places: &ext::blob::Places,
    ) -> io::Result<Arc<ext::host::Registry>> {
        let image: crate::context::PresentationImage = crate::context::presentation(cut, places)?
            .parse()
            .map_err(io::Error::other)?;
        let source = self.source(workspace)?;
        let registry = super::recorded_registry(self, source.as_ref(), Some(&image.installed))?;
        let registry = if registry
            .extensions()
            .map(|(name, _)| name)
            .eq(image.extensions.iter().map(String::as_str))
        {
            registry
        } else {
            Arc::new(
                registry
                    .select(ext::Inheritance::All, Some(&image.extensions))
                    .map_err(io::Error::other)?,
            )
        };
        if !registry
            .extensions()
            .map(|(name, _)| name)
            .eq(image.extensions.iter().map(String::as_str))
        {
            return Err(io::Error::other("frozen presentation registrations differ"));
        }
        Ok(registry)
    }
}

#[derive(Default)]
pub(super) struct Projection {
    pub(super) admissions: super::spawn::Admissions,
    pub(super) active: Option<super::prefix::Active>,
    pub(super) forks: Option<super::prefix::Projection>,
    pub(super) seeds: std::collections::BTreeMap<protocol::BlobId, Vec<super::prefix::Seed>>,
    pub(super) compiler: Option<(std::sync::Weak<ext::host::Registry>, super::Json)>,
    pub(super) transcript: Option<Transcript>,
    pub(super) invocations: Invocations,
    /// The message of the reply model call `.0` just had, until its
    /// result commits.
    pub(super) replied: Option<(CallId, llm::Assistant)>,
    pub(super) executions: Vec<(CallId, super::Json)>,
    pub(super) published_activity: Option<(u64, Activity)>,
}

impl Projection {
    pub(super) fn release_model(&mut self) {
        self.transcript = None;
        self.active = None;
        self.forks = None;
        self.seeds.clear();
        self.compiler = None;
    }

    pub(super) fn compiler_scope(&mut self, registry: &Arc<ext::host::Registry>) -> super::Json {
        if let Some((held, scope)) = &self.compiler
            && held
                .upgrade()
                .is_some_and(|held| Arc::ptr_eq(&held, registry))
        {
            return scope.clone();
        }
        let scope = super::prefix::scope(registry);
        self.compiler = Some((Arc::downgrade(registry), scope.clone()));
        scope
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Invocations {
    through: u64,
    calls: std::collections::BTreeMap<CallId, Invocation>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Invocation {
    key: protocol::Invocation,
    tool: String,
    input: super::Json,
    completed: bool,
}

impl Invocations {
    pub(super) fn fold(&mut self, entry: &Entry) -> Option<CallId> {
        if entry.seq.get() <= self.through {
            return None;
        }
        self.through = entry.seq.get();
        match &entry.body {
            Body::ToolCall {
                invoke: Some(key),
                tool,
                input,
                ..
            } => {
                self.calls.insert(
                    CallId(entry.seq),
                    Invocation {
                        key: key.clone(),
                        tool: tool.clone(),
                        input: input.clone(),
                        completed: false,
                    },
                );
                return Some(CallId(entry.seq));
            }
            Body::ToolResult { call, .. } => {
                self.calls
                    .retain(|_, invocation| invocation.key.parent != *call);
                if let Some(invocation) = self.calls.get_mut(call) {
                    invocation.completed = true;
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn request(
        &self,
        parent: CallId,
        name: &str,
        tool: &str,
        input: &super::Json,
    ) -> Result<Option<CallId>, String> {
        if name.is_empty() {
            return Err("invocation needs a name".into());
        }
        if let Some((call, invocation)) = self
            .calls
            .iter()
            .find(|(_, invocation)| invocation.key.parent == parent && invocation.key.name == name)
        {
            return if invocation.tool == tool && &invocation.input == input {
                Ok(Some(*call))
            } else {
                Err("invocation name was already used with different arguments".into())
            };
        }
        let mut ancestor = parent;
        for _ in 1..8 {
            let Some(invocation) = self.calls.get(&ancestor) else {
                return Ok(None);
            };
            ancestor = invocation.key.parent;
        }
        if self.calls.contains_key(&ancestor) {
            Err("tool delegation is too deep".into())
        } else {
            Ok(None)
        }
    }

    pub(super) fn completed(&self, call: CallId) -> bool {
        self.calls
            .get(&call)
            .is_some_and(|invocation| invocation.completed)
    }
}

impl super::Waiter {
    fn reply(self, body: &Body) {
        let interrupted = || protocol::Outcome::Interrupted { output: None };
        match (self, body) {
            (
                Self::Tool(reply),
                Body::ToolResult {
                    outcome,
                    card,
                    held,
                    ..
                },
            ) => reply.send(super::Called {
                outcome: outcome.clone(),
                card: card.clone(),
                held: held.clone(),
                parked: None,
            }),
            (Self::Tool(reply), Body::Interrupt { .. }) => reply.send(super::Called {
                outcome: interrupted().into(),
                card: None,
                held: Vec::new(),
                parked: None,
            }),
            (
                Self::Model(reply),
                Body::ModelResult {
                    outcome, failure, ..
                },
            ) => reply.send(super::Opened::Cached {
                outcome: outcome.clone(),
                failure: failure.clone(),
            }),
            (Self::Model(reply), Body::Interrupt { .. }) => reply.send(super::Opened::Cached {
                outcome: interrupted(),
                failure: None,
            }),
            _ => unreachable!("committed result"),
        }
    }
}

impl Actor {
    pub(super) async fn invocation_reply(
        &mut self,
        ticket: super::Ticket,
        result: Result<CallId, String>,
    ) {
        if let Some((reply, _)) = self.replies.invocations.remove(&ticket) {
            match result {
                Ok(call) => {
                    if self.indexed().await.is_ok() {
                        self.await_reply(call, super::Waiter::Tool(reply)).await;
                    }
                }
                Err(error) => reply.send(super::Called::failed(error)),
            }
        }
    }

    pub(super) async fn await_reply(&mut self, call: CallId, waiter: super::Waiter) {
        if let Some(seq) = self.journal.index.result(call, self.journal.durable_seq) {
            if let Ok(entry) = self.indexed_entry(seq).await {
                self.replies.contexts.remove(&call);
                waiter.reply(&entry.body);
            }
        } else {
            self.replies.invoked.entry(call).or_default().push(waiter);
        }
    }

    pub(super) async fn show(&mut self, entries: Vec<Entry>) {
        if entries.is_empty() {
            return;
        }
        for e in &entries {
            self.journal.index.apply(e);
            let call = match &e.body {
                Body::ToolResult { call, .. } | Body::ModelResult { call, .. } => Some(*call),
                Body::Interrupt { .. } => Some(CallId(e.seq)),
                _ => None,
            };
            if let Some(call) = call {
                let mut waiters = self.replies.invoked.remove(&call).unwrap_or_default();
                if let Some(input) = self.journal.index.input(call) {
                    waiters.extend(self.replies.invoked.remove(&input).unwrap_or_default());
                }
                self.replies.contexts.remove(&call);
                for waiter in waiters {
                    waiter.reply(&e.body);
                }
            }
        }
        let frames = Arc::clone(&self.live.frames);
        let indexed = frames.history.is_indexed();
        let publish = move || {
            for entry in entries {
                frames.push(entry.seq.get());
                if indexed {
                    frames.history.observe(&entry);
                }
            }
        };
        if indexed {
            rt::unblock_with(
                &*self.live.session.shared.rt,
                Cpu,
                rt::STORAGE_BYTES,
                publish,
            )
            .await;
        } else {
            publish();
        }
        self.live.shown();
        lock(&self.live.followers).retain(|c| c.wake().is_ok());
    }

    pub(super) fn folded(&mut self, entries: &[Entry]) {
        let session = &self.live.session;
        let hooks = self.registry.hooks();
        self.projection.transcript = self.projection.transcript.take().filter(|t| {
            t.is_on(&self.registry) && !entries.iter().any(|e| matches!(e.body, Body::Edit { .. }))
        });
        for e in entries {
            let _ = self.projection.invocations.fold(e);
            if let Body::Answer { call, .. }
            | Body::Decision {
                call: Some(call),
                d: protocol::Verdict::Approval { .. },
                ..
            } = &e.body
            {
                for (owner, side) in &self.waiting_sides {
                    if owner == call {
                        session.cancel(*side).set();
                    }
                }
            }
            if let Body::ModelResult { call, .. } = &e.body {
                self.waiting_sides.retain(|(_, side)| side != call);
            }
            lock(&session.state_view()).apply(e);
            if matches!(e.body, Body::Input { .. }) {
                self.live.has_input.store(true, Ordering::SeqCst);
            }

            // What observers do about it runs owned by the session.
            let tool = match &e.body {
                Body::Decision {
                    call: Some(call), ..
                } => self.core.tool_of(*call),
                _ => None,
            };
            let point = self.bound.point();
            let point = match &tool {
                Some((tool, input)) => point.with_tool(ext::point::ToolUse { tool, input }),
                None => point,
            };
            for reaction in (!session.model_only())
                .then(|| hooks.observe(point, e))
                .into_iter()
                .flatten()
            {
                if let Some(work) = reaction.work {
                    self.admissions.push(super::Guarded {
                        admission: Admission {
                            through: e.seq.get(),
                            requirement: reaction.requirement,
                        },
                        work: Admitted::Reaction(work),
                    });
                }
            }
            let replied = match &e.body {
                Body::ModelResult { call, .. } => {
                    self.projection.replied.take_if(|(of, _)| of == call)
                }
                _ => None,
            };
            // Naming or reading what an entry references is file I/O, so
            // the transcript stops short of it, and the next projection
            // goes on from there on a blocking worker; as it does from
            // any entry it stopped short of before.
            if self
                .projection
                .transcript
                .as_mut()
                .is_some_and(|transcript| {
                    transcript.through() + 1 == e.seq.get()
                        && (e.body.blobs().iter().all(|held| matches!(held, protocol::Held::Blob(blob) if blob.origin == protocol::BlobOrigin::Context)))
                        && !transcript.apply(e, replied.map(|(_, m)| m))
                })
            {
                self.projection.transcript = None;
            }
            // A call ends with its result, unless it was detached: then
            // with its own envelope, its result a placeholder.
            let ended = match &e.body {
                Body::ToolResult { call, .. } | Body::ModelResult { call, .. } => {
                    self.bound.settled(*call);
                    let ledger = self.core.ledger();
                    (!ledger.detached().iter().any(|d| d.call == *call)).then_some(*call)
                }
                Body::Enqueue { envelope }
                    if envelope.id().purpose == protocol::EnvelopeId::DETACHED
                        && envelope.source() == protocol::Source::Job(envelope.id().call) =>
                {
                    Some(envelope.id().call)
                }
                _ => None,
            };
            if let Some(call) = ended {
                self.inflight.remove(&call);
                session.cancelled(call);
            }
        }
    }

    /// Lists the visible prefix and certifies its activity only when the
    /// whole ledger is visible. Eligibility can change without the summary.
    pub(super) fn publish(&mut self) {
        let seq = self.live.frames.seq();
        let activity = self.core.ledger().activity();
        let changed = {
            let mut summary = lock(&self.live.summary);
            let changed = summary.entries != seq || summary.activity != activity;
            summary.entries = seq;
            summary.activity = activity;
            changed
        };
        if changed && let Some(daemon) = self.live.session.shared.daemon.upgrade() {
            daemon.changed(self.live.id);
            daemon.pulse.notify();
        }
        let current = self.current_activity();
        if self.projection.published_activity == current {
            return;
        }
        if let Some((seq, activity)) = current
            && let Ok(frame) = Frame::new(&DaemonMsg::Activity { seq, activity })
        {
            lock(&self.live.followers).retain(|c| c.send_after(seq, frame.clone()).is_ok());
            self.projection.published_activity = current;
        }
    }

    pub(super) fn current_activity(&self) -> Option<(u64, Activity)> {
        let seq = self.core.ledger().seq();
        (self.live.frames.seq() == seq && self.journal.durable_seq >= seq)
            .then(|| (seq, self.core.ledger().activity()))
    }

    pub(super) async fn attach(&mut self, client: &Outbox, since: u64) {
        let live = Arc::clone(&self.live);
        let frames = Arc::clone(&live.frames);
        let sides = client.sides();
        let head = rt::try_unblock_with(
            &*live.session.shared.rt,
            Cpu,
            rt::STORAGE_BYTES,
            move || {
                let head = frames.history.head_for(&frames, since, &sides)?;
                let seq = head.cut.seq;
                head.frames().map(|frames| (seq, frames))
            },
        )
        .await;
        let (seq, head) = match head {
            Ok(Ok(head)) => head,
            Ok(Err(error)) | Err(error) => {
                let _ = client.send(
                    Frame::new(&DaemonMsg::Error {
                        message: error.to_string(),
                    })
                    .expect("an error frames"),
                );
                return;
            }
        };
        let mut frames = Vec::new();
        frames.extend(
            Frame::new(&DaemonMsg::Attached {
                session: live.id,
                workspace: protocol::HostPath::lossy(&live.workspace),
            })
            .ok()
            .map(|f| (0, f)),
        );
        if let Some((seq, activity)) = self.current_activity() {
            frames.extend(
                Frame::new(&DaemonMsg::Activity { seq, activity })
                    .ok()
                    .map(|f| (seq, f)),
            );
        }
        frames.extend(
            Frame::new(&DaemonMsg::Synced { seq })
                .ok()
                .map(|f| (seq, f)),
        );
        let follower = client.follow_head(Arc::clone(&live.frames), head, frames);
        lock(&live.followers).push(&follower);
    }

    fn canonical_source(
        &self,
        call: CallId,
    ) -> Arc<dyn Fn() -> Result<llm::Messages, llm::Error> + Send + Sync> {
        history_reader(
            Arc::clone(&self.live.frames),
            &self.registry,
            self.pending_entries().collect(),
            self.live.session.blobs.places(),
            self.live.session.model_only(),
            HistoryCut {
                through: self.journal.ordered_seq,
                call,
                exchanges: None,
            },
        )
    }

    pub(super) async fn bounded_history(
        &mut self,
        call: CallId,
        exchanges: u16,
    ) -> io::Result<llm::Messages> {
        let cut = self.context_cut(call.get() - 1).await?;
        let registry = Arc::clone(&self.registry);
        let places = self.live.session.blobs.places();
        rt::unblock(&*self.live.session.shared.rt, move || {
            Transcript::cut(&cut, registry, "", &places, Some(exchanges))
                .map(Transcript::into_messages)
        })
        .await
    }

    pub(super) async fn context_cut(
        &mut self,
        through: u64,
    ) -> io::Result<protocol::publication::ContextImage> {
        let session = Arc::clone(&self.live.session);
        let frames = Arc::clone(&self.live.frames);
        let pending: Vec<_> = self.pending_entries().collect();
        rt::unblock(&*self.live.session.shared.rt, move || {
            #[cfg(test)]
            crate::context::register_test_places(session.blobs.places());
            let mut cuts = lock(&session.context_cuts);
            let (after, previous) = cuts.before(through);
            if after == through {
                return Ok(previous);
            }
            let mut entries = Vec::new();
            'committed: for batch in frames.batches_after(after) {
                for entry in batch? {
                    if entry.seq.get() > through {
                        break 'committed;
                    }
                    entries.push(entry);
                }
            }
            let committed = entries.last().map_or(after, |entry| entry.seq.get());
            entries.extend(
                pending
                    .into_iter()
                    .filter(|entry| entry.seq.get() > committed && entry.seq.get() <= through),
            );
            let image = crate::context::append(previous, entries, &*session.shared.blobs)?;
            cuts.insert(through, image.clone());
            Ok(image)
        })
        .await
    }

    /// The conversation the model sees before entry `call`, extended on a CPU worker.
    pub(super) async fn conversation(&mut self, call: CallId) -> io::Result<llm::Conversation> {
        let restore = self.canonical_source(call);
        if let Some(messages) = (self.projection.transcript.as_ref())
            .filter(|t| t.through() + 1 >= call.get())
            .and_then(|t| {
                let restore = Arc::clone(&restore);
                t.request_before(call.get(), move || restore())
            })
        {
            return Ok(messages);
        }
        let frames = Arc::clone(&self.live.frames);
        let registry = Arc::clone(&self.registry);
        let pending: Vec<_> = self.pending_entries().collect();
        let model_only = self.live.session.model_only();
        let places = self.live.session.blobs.places();
        let kept = self.projection.transcript.take();
        let seed = lock(&self.live.session.incoming_prefix).take();
        let (transcript, messages) = rt::try_unblock_with(
            &*self.live.session.shared.rt,
            Cpu,
            rt::STORAGE_BYTES,
            move || {
                let system = if model_only {
                    ""
                } else {
                    registry.system_prompt()
                };
                let mut entries = Vec::new();
                if let Some(transcript) = &kept {
                    for batch in frames.batches_after(transcript.through()) {
                        entries.extend(batch?);
                    }
                    entries.extend(
                        pending
                            .iter()
                            .filter(|e| e.seq.get() > transcript.through())
                            .cloned(),
                    );
                }
                let advanced = kept.and_then(|mut transcript| {
                    entries
                        .iter()
                        .all(|entry| transcript.apply(entry, None))
                        .then_some(transcript)
                });
                let transcript = if let Some(transcript) = advanced {
                    transcript
                } else if let Some(seed) = seed
                    .filter(|seed| seed.prefix.scope() == Some(&super::prefix::scope(&registry)))
                {
                    let entries: Vec<_> = frames
                        .batches_after(0)
                        .collect::<io::Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .chain(pending.iter().cloned())
                        .collect();
                    Transcript::replay_seeded(
                        &entries,
                        Arc::clone(&registry),
                        system,
                        &places,
                        seed,
                    )?
                } else {
                    replay_frames(
                        &frames,
                        &pending,
                        Arc::clone(&registry),
                        system,
                        &places,
                        None,
                    )?
                };
                if let Some(messages) = transcript.request_before(call.get(), move || restore()) {
                    return io::Result::Ok((transcript, messages));
                }
                let messages = replay_frames(
                    &frames,
                    &pending,
                    Arc::clone(&registry),
                    system,
                    &places,
                    Some(call),
                )?
                .into_messages();
                io::Result::Ok((transcript, messages.into()))
            },
        )
        .await??;
        self.projection.transcript = Some(transcript);
        Ok(messages)
    }
}

fn replay_frames(
    frames: &super::FrameLog,
    pending: &[Entry],
    registry: Arc<ext::host::Registry>,
    system: &str,
    places: &ext::blob::Places,
    before: Option<CallId>,
) -> io::Result<Transcript> {
    Transcript::replay(
        |each| {
            let mut visit = |entry: &Entry| {
                if before.is_none_or(|call| {
                    entry.seq < call.0
                        || matches!(entry.body, Body::Context { .. } | Body::Edit { .. })
                }) {
                    each(entry);
                }
            };
            for batch in frames.batches_after(0) {
                for entry in batch? {
                    visit(&entry);
                }
            }
            for entry in pending {
                visit(entry);
            }
            Ok(())
        },
        registry,
        system,
        places,
    )
}

#[derive(Clone, Copy)]
struct HistoryCut {
    through: u64,
    call: CallId,
    exchanges: Option<u16>,
}

fn history_reader(
    source_frames: Arc<super::FrameLog>,
    registry: &Arc<ext::host::Registry>,
    source_pending: Vec<Entry>,
    source_places: ext::blob::Places,
    model_only: bool,
    cut: HistoryCut,
) -> Arc<dyn Fn() -> Result<llm::Messages, llm::Error> + Send + Sync> {
    let HistoryCut {
        through: source_through,
        call,
        exchanges: context_exchanges,
    } = cut;
    let source_committed = source_frames.seq();
    let source_registry = Arc::downgrade(registry);
    Arc::new(move || {
        let source_registry = source_registry.upgrade().ok_or_else(|| {
            llm::Error::new(
                llm::Kind::Other,
                "canonical history's registry generation has retired",
            )
        })?;
        Transcript::replay_bounded(
            |each| {
                'history: for batch in source_frames.batches_after(0) {
                    for entry in batch? {
                        if entry.seq.get() > source_committed {
                            break 'history;
                        }
                        if entry.seq < call.0
                            || matches!(entry.body, Body::Context { .. } | Body::Edit { .. })
                        {
                            each(&entry);
                        }
                    }
                }
                for entry in &source_pending {
                    if entry.seq.get() <= source_through
                        && (entry.seq < call.0
                            || matches!(entry.body, Body::Context { .. } | Body::Edit { .. }))
                    {
                        each(entry);
                    }
                }
                Ok(())
            },
            Arc::clone(&source_registry),
            if model_only {
                ""
            } else {
                source_registry.system_prompt()
            },
            &source_places,
            context_exchanges,
        )
        .map(Transcript::into_messages)
        .map_err(|error| llm::Error::new(llm::Kind::Other, error.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Audience, Epoch, Json, Outcome, Seq};

    #[test]
    fn saved_history_readers_replay_pending_entries_once_after_flush() {
        use crate::store::{AppendMode, Header, Jsonl, LogStore};

        let dir = tempfile::tempdir().unwrap();
        let store = Jsonl::open(dir.path()).unwrap();
        let mut loaded = store
            .create(&Header {
                id: protocol::SessionId::new(),
                workspace: dir.path().into(),
                parent: None,
            })
            .unwrap();
        loaded
            .append(Body::Assistant {
                text: "committed".into(),
            })
            .unwrap();
        let frames = Arc::new(super::super::FrameLog::new(
            Arc::clone(&loaded.lines),
            loaded.seq,
        ));
        let mut pending = [Entry::new(
            Seq::new(loaded.seq + 1).unwrap(),
            Epoch(0),
            Body::Assistant {
                text: "pending".into(),
            },
        )];
        loaded
            .writer
            .append(&mut pending, AppendMode::Write)
            .unwrap();
        let registry = Arc::new(ext::host::Registry::default());
        let reader = history_reader(
            Arc::clone(&frames),
            &registry,
            pending.to_vec(),
            ext::blob::Places::default(),
            true,
            HistoryCut {
                through: pending[0].seq.get(),
                call: CallId::new(pending[0].seq.get() + 1).unwrap(),
                exchanges: None,
            },
        );
        let before = reader().unwrap();
        assert!(Json::encode(&before).unwrap().raw().contains("pending"));
        loaded.writer.flush_inline().unwrap();
        frames.push(pending[0].seq.get());
        assert_eq!(reader().unwrap(), before);
    }

    #[test]
    fn batched_snapshot_folding_matches_shared_view_vectors() {
        use protocol::Decode as _;

        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../protocol/tests/vectors/view.json")).unwrap();
        for vector in vectors.as_array().unwrap() {
            let mut view = SessionView::default();
            for message in vector["messages"].as_array().unwrap() {
                let message = DaemonMsg::from_json(message.to_string().as_bytes()).unwrap();
                match message {
                    DaemonMsg::Entry(entry) => view.extend([entry]),
                    message => {
                        view.fold(&message);
                    }
                }
            }
            assert_eq!(
                format!("{:016x}", view.state_hash()),
                vector["hash"].as_str().unwrap(),
                "{}",
                vector["name"]
            );
        }
    }

    #[test]
    fn delegated_completion_identity_survives_checkpoints() {
        let parent = CallId::new(1).unwrap();
        let call = CallId::new(2).unwrap();
        let input = Json::encode(&"input").unwrap();
        let mut live = Invocations::default();
        live.fold(&Entry::new(
            call.0,
            Epoch(0),
            Body::ToolCall {
                invoke: Some(protocol::Invocation {
                    parent,
                    name: "effect".into(),
                }),
                tool: "write".into(),
                input: input.clone(),
                id: None,
                card: None,
                by: Audience::Model,
            },
        ));
        for outcome in [
            Outcome::Done {
                output: Json::encode(&"result").unwrap().into(),
            },
            Outcome::Failed {
                error: "failed".into(),
            },
            Outcome::Interrupted {
                output: Some("partial".into()),
            },
        ] {
            let mut completed = live.clone();
            completed.fold(&Entry::new(
                Seq::new(3).unwrap(),
                Epoch(0),
                Body::ToolResult {
                    call,
                    outcome: outcome.clone(),
                    card: None,
                    held: Vec::new(),
                },
            ));
            let mut restored: Invocations = Json::encode(&completed).unwrap().parse().unwrap();
            assert_eq!(
                restored.request(parent, "effect", "write", &input),
                Ok(Some(call))
            );
            assert!(restored.completed(call));
            let end = Entry::new(
                Seq::new(4).unwrap(),
                Epoch(0),
                Body::ToolResult {
                    call: parent,
                    outcome: Outcome::Interrupted { output: None },
                    card: None,
                    held: Vec::new(),
                },
            );
            restored.fold(&end);
            completed.fold(&end);
            assert!(!restored.completed(call));
            assert!(!completed.completed(call));
            assert!(
                restored
                    .request(parent, "effect", "write", &input)
                    .unwrap()
                    .is_none()
            );
        }
    }
}
