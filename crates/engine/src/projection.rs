//! Speculative model context and durable client views have separate prefixes.

use super::{
    Activity, Actor, Admission, Admitted, Arc, Body, CallId, Cpu, DaemonMsg, Encoded, Entry, Frame,
    Ordering, Outbox, SNAPSHOT_GAP, SessionView, Transcript, io, lock,
};

/// A prefix coupled to the authority admitted by its own session.
pub(super) struct OwnerImage {
    entries: Vec<Entry>,
    registry: Arc<super::Registry>,
}

impl super::Shared {
    pub(super) fn owner_image(
        &self,
        owner: super::SessionId,
        through: u64,
    ) -> io::Result<OwnerImage> {
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
        Ok(OwnerImage { entries, registry })
    }
}

impl OwnerImage {
    pub(super) fn project(self) -> llm::Messages {
        let system = self.registry.system_prompt();
        Transcript::of(&self.entries, Arc::clone(&self.registry), system).into_messages()
    }
}

#[derive(Default)]
pub(super) struct Projection {
    pub(super) transcript: Option<Transcript>,
    pub(super) invocations: Invocations,
    /// The message of the reply model call `.0` just had, until its
    /// result commits.
    pub(super) replied: Option<(CallId, llm::Assistant)>,
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
        let interrupted = protocol::Outcome::Interrupted { output: None };
        let (outcome, failure, card) = match body {
            Body::ToolResult { outcome, card, .. } => (outcome, None, card.as_ref()),
            Body::ModelResult {
                outcome, failure, ..
            } => (outcome, failure.as_ref(), None),
            Body::Interrupt { .. } => (&interrupted, None, None),
            _ => unreachable!("committed result"),
        };
        match self {
            Self::Tool(reply) => reply.send(super::Called {
                outcome: outcome.clone(),
                card: card.cloned(),
            }),
            Self::Model(reply) => reply.send(super::Opened::Cached {
                outcome: outcome.clone(),
                failure: failure.cloned(),
            }),
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
            if let Some(transcript) = &mut self.projection.transcript {
                transcript.apply(e, replied.map(|(_, m)| m));
            }
            if let Body::ToolResult { call, .. } | Body::ModelResult { call, .. } = &e.body {
                self.inflight.remove(call);
                session.cancelled(*call);
                self.bound.settled(*call);
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
            workspace: live.workspace.clone(),
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
        lock(&live.followers).push(follower);
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
        let (view, frame) = rt::unblock_with(
            &*self.live.session.shared.rt,
            Cpu,
            rt::STORAGE_BYTES,
            move || {
                let view = match view {
                    Some(view) => view,
                    None => SessionView::from_log(
                        live.id,
                        live.workspace.clone(),
                        live.frames.entries().ok()?,
                    ),
                };
                let frame = Frame::new(&DaemonMsg::Snapshot(Box::new(view.snapshot()))).ok();
                Some((view, frame))
            },
        )
        .await?;
        self.projection.view = Some(view);
        self.projection.snapshot = Some((seq, frame.clone()));
        frame
    }

    /// The conversation the model sees before entry `call`.
    pub(super) async fn conversation(&mut self, call: CallId) -> io::Result<llm::Messages> {
        if let Some(messages) = self
            .projection
            .transcript
            .as_ref()
            .and_then(|t| t.before(call.get()))
        {
            return Ok(messages.clone());
        }
        let frames = Arc::clone(&self.live.frames);
        let registry = Arc::clone(&self.registry);
        let pending: Vec<_> = self.pending_entries().collect();
        let model_only = self.live.session.model_only();
        let (transcript, messages) = rt::unblock_with(
            &*self.live.session.shared.rt,
            Cpu,
            rt::STORAGE_BYTES,
            move || {
                let entries: Vec<_> = frames.entries()?.into_iter().chain(pending).collect();
                let system = if model_only {
                    ""
                } else {
                    registry.system_prompt()
                };
                let transcript =
                    Transcript::of(entries.iter().cloned(), Arc::clone(&registry), system);
                let messages = transcript.before(call.get()).cloned().unwrap_or_else(|| {
                    Transcript::of(
                        entries.into_iter().filter(|e| {
                            e.seq < call.0
                                || matches!(e.body, Body::Context { .. } | Body::Edit { .. })
                        }),
                        Arc::clone(&registry),
                        system,
                    )
                    .into_messages()
                });
                io::Result::Ok((transcript, messages))
            },
        )
        .await?;
        self.projection.transcript = Some(transcript);
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Audience, Epoch, Json, Outcome, Seq};

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
                output: Json::encode(&"result").unwrap(),
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
