//! Speculative model context and durable client views have separate prefixes.

use super::{
    Activity, Actor, Admission, Admitted, Arc, Body, CallId, Cpu, DaemonMsg, Encoded, Entry, Frame,
    Ordering, Outbox, SNAPSHOT_GAP, SessionView, Transcript, io, lock,
};

impl super::Shared {
    pub(super) fn owner_image(
        &self,
        owner: super::SessionId,
        through: u64,
    ) -> io::Result<llm::Messages> {
        let header = self
            .catalog
            .get(owner)
            .ok_or_else(|| io::Error::other("missing context owner"))?
            .header;
        let entries = self.store.prefix(owner, through)?;
        let source = self.source(&header.workspace)?;
        let installed = entries.iter().rev().find_map(|e| match &e.body {
            Body::Extensions { installed } => Some(installed.as_slice()),
            _ => None,
        });
        let current = super::recorded_registry(self, source.as_ref(), installed)?;
        let registry = if header.parent.is_some() {
            super::spawn::restore(&header, self, super::Open::Load, source.as_ref(), current)?.0
        } else {
            current
        };
        let system = registry.system_prompt();
        // Named and read as the owner's own transcript does: a payload its
        // log holds as a blob is read back from the store.
        let blobs = ext::blob::Blobs::new(Arc::clone(&self.blobs), Arc::clone(&self.tree), owner);
        let places = blobs.places();
        Ok(Transcript::placed(&entries, Arc::clone(&registry), system, &places).into_messages())
    }
}

#[derive(Default)]
pub(super) struct Projection {
    pub(super) transcript: Option<Transcript>,
    pub(super) invocations: Invocations,
    /// The message of the reply model call `.0` just had, until its
    /// result commits.
    pub(super) replied: Option<(CallId, llm::Assistant)>,
    pub(super) executions: Vec<(CallId, super::Json)>,
    pub(super) snapshot: Option<(u64, Option<Frame>)>,
    /// The view a far attach is sent, kept in step with each commit from
    /// the first time one is asked for: no attach folds the whole log.
    pub(super) view: Option<SessionView>,
    pub(super) published_activity: Option<(u64, Activity)>,
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
            }),
            (Self::Tool(reply), Body::Interrupt { .. }) => reply.send(super::Called {
                outcome: interrupted().into(),
                card: None,
                held: Vec::new(),
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

    pub(super) fn show(&mut self, entries: &[Entry], lines: &Encoded) {
        if let Some(view) = &mut self.projection.view {
            view.extend(entries);
        }
        for (e, line) in entries.iter().zip(lines.lines()) {
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
            // One too large for the tail is not framed: followers read it.
            let frame = (line.len() <= crate::outbox::TAIL)
                .then(|| Frame::from_line(e.seq.get(), line).ok())
                .flatten();
            self.live.frames.push(e.seq.get(), frame);
        }
        self.live.shown();
        lock(&self.live.followers).retain(|c| c.wake().is_ok());
    }

    pub(super) fn folded(&mut self, entries: &[Entry]) {
        let session = &self.live.session;
        let hooks = self.registry.hooks();
        self.projection.transcript = self
            .projection
            .transcript
            .take()
            .filter(|t| t.is_on(&self.registry));
        // An edit changes what came before it: the transcript is made again.
        if entries.iter().any(|e| matches!(e.body, Body::Edit { .. })) {
            self.projection.transcript = None;
        }
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
            if let Some(transcript) = &mut self.projection.transcript
                && transcript.through() + 1 == e.seq.get()
                && e.body.blobs().is_empty()
            {
                transcript.apply(e, replied.map(|(_, m)| m));
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
        {
            let mut summary = lock(&self.live.summary);
            summary.entries = seq;
            summary.activity = activity;
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
        let seq = self.live.frames.seq();
        let far = seq.saturating_sub(since) > SNAPSHOT_GAP;
        let snapshot = if far { self.snapshot(seq).await } else { None };
        let cursor = if snapshot.is_some() {
            seq
        } else {
            since.min(seq)
        };
        let live = &self.live;
        let mut frames = Vec::new();
        let attached = DaemonMsg::Attached {
            session: live.id,
            workspace: protocol::HostPath::lossy(&live.workspace),
        };
        frames.extend(Frame::new(&attached).ok().map(|f| (cursor, f)));
        frames.extend(snapshot.map(|frame| (seq, frame)));
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
        let follower = client.follow(Arc::clone(&live.frames), cursor, frames);
        lock(&live.followers).push(&follower);
    }

    /// The snapshot at entry `seq`, the last committed, made once for every
    /// client attaching then; `None` if too large for a frame.
    pub(super) async fn snapshot(&mut self, seq: u64) -> Option<Frame> {
        if let Some((at, frame)) = &self.projection.snapshot
            && *at == seq
        {
            return frame.clone();
        }
        let live = Arc::clone(&self.live);
        let view = self.projection.view.take();
        let (view, frame) = rt::try_unblock_with(
            &*self.live.session.shared.rt,
            Cpu,
            rt::STORAGE_BYTES,
            move || {
                let view = if let Some(view) = view {
                    view
                } else {
                    let mut view = SessionView::from_log(
                        live.id,
                        protocol::HostPath::lossy(&live.workspace),
                        std::iter::empty::<Entry>(),
                    );
                    for batch in live.frames.batches_after(0) {
                        view.extend(batch.ok()?);
                    }
                    view
                };
                let frame = Frame::new(&DaemonMsg::Snapshot(Box::new(view.snapshot()))).ok();
                Some((view, frame))
            },
        )
        .await
        .ok()??;
        self.projection.view = Some(view);
        self.projection.snapshot = Some((seq, frame.clone()));
        frame
    }

    /// The conversation the model sees before entry `call`: the
    /// transcript as it stands, gone on with on a blocking worker from
    /// where it stopped short, or, with none, made from the whole log.
    pub(super) async fn conversation(&mut self, call: CallId) -> io::Result<llm::Messages> {
        if let Some(messages) = (self.projection.transcript.as_ref())
            .filter(|t| t.through() + 1 >= call.get())
            .and_then(|t| t.before(call.get()))
        {
            return Ok(messages.clone());
        }
        let frames = Arc::clone(&self.live.frames);
        let registry = Arc::clone(&self.registry);
        let pending: Vec<_> = self.pending_entries().collect();
        let model_only = self.live.session.model_only();
        let places = self.live.session.blobs.places();
        let kept = self.projection.transcript.take();
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
                // Each entry is read once: what was folded stays.
                let transcript = if let Some(mut transcript) = kept {
                    let through = transcript.through();
                    for batch in frames.batches_after(through) {
                        for entry in batch? {
                            transcript.apply(&entry, None);
                        }
                    }
                    for entry in pending.iter().filter(|e| e.seq.get() > through) {
                        transcript.apply(entry, None);
                    }
                    transcript
                } else {
                    Transcript::replay(
                        |each| {
                            for batch in frames.batches_after(0) {
                                for entry in batch? {
                                    each(&entry);
                                }
                            }
                            for entry in &pending {
                                each(entry);
                            }
                            Ok(())
                        },
                        Arc::clone(&registry),
                        system,
                        &places,
                    )?
                };
                if let Some(messages) = transcript.before(call.get()).cloned() {
                    return io::Result::Ok((transcript, messages));
                }
                let messages = Transcript::replay(
                    |each| {
                        let mut before = |entry: &Entry| {
                            if entry.seq < call.0
                                || matches!(entry.body, Body::Context { .. } | Body::Edit { .. })
                            {
                                each(entry);
                            }
                        };
                        for batch in frames.batches_after(0) {
                            for entry in batch? {
                                before(&entry);
                            }
                        }
                        for entry in &pending {
                            before(entry);
                        }
                        Ok(())
                    },
                    Arc::clone(&registry),
                    system,
                    &places,
                )?
                .into_messages();
                io::Result::Ok((transcript, messages))
            },
        )
        .await??;
        self.projection.transcript = Some(transcript);
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Audience, Epoch, Json, Outcome, Seq};

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
