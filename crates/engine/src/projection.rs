//! Speculative model context and durable client views have separate prefixes.

use super::{
    Activity, Actor, Admission, Admitted, Arc, Body, CallId, Cpu, DaemonMsg, Encoded, Entry, Frame,
    Opened, Ordering, Outbox, SNAPSHOT_GAP, SessionView, Transcript, io, lock,
};

#[derive(Default)]
pub(super) struct Projection {
    pub(super) transcript: Option<Transcript>,
    /// The message of the reply model call `.0` just had, until its
    /// result commits.
    pub(super) replied: Option<(CallId, llm::Assistant)>,
    pub(super) snapshot: Option<(u64, Option<Frame>)>,
    /// The view a far attach is sent, kept in step with each commit from
    /// the first time one is asked for: no attach folds the whole log.
    pub(super) view: Option<SessionView>,
    pub(super) published_activity: Option<(u64, Activity)>,
}

impl Actor {
    pub(super) fn show(&mut self, entries: &[Entry], lines: &Encoded) {
        if let Some(view) = &mut self.projection.view {
            view.extend(entries);
        }
        for (e, line) in entries.iter().zip(lines.lines()) {
            if let Body::ModelResult {
                call,
                outcome,
                failure,
                ..
            } = &e.body
                && let Some(waiters) = self.side_waiters.remove(call)
            {
                for waiter in waiters {
                    waiter.send(Opened::Cached {
                        outcome: outcome.clone(),
                        failure: failure.clone(),
                    });
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
            for reaction in hooks.observe(point, e) {
                if let Some(work) = reaction.work {
                    self.admissions.push((
                        Admission {
                            through: e.seq.get(),
                            requirement: reaction.requirement,
                        },
                        Admitted::Reaction(work),
                    ));
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
                session.forget(*call);
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
                let frame = Frame::new(&DaemonMsg::Snapshot(Box::new(view.clone()))).ok();
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
        let (transcript, messages) = rt::unblock_with(
            &*self.live.session.shared.rt,
            Cpu,
            rt::STORAGE_BYTES,
            move || {
                let entries: Vec<_> = frames.entries()?.into_iter().chain(pending).collect();
                let system = registry.system_prompt();
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
