//! A session's side of a handover ([`crate::handover`]). Freezing stops
//! every group its work runs, between two chunks of output, and the session
//! with them: it takes nothing more up, so no call it stopped can end and
//! no turn start. The daemon commits only once every session froze, and
//! then each gives its groups up for good; until then a thaw calls it off,
//! every group supervised again and what came meanwhile taken up in order.
//!
//! The build after takes up what it was handed as the session loads: a call
//! goes on through its tool ([`Work::Adopt`](kernel::Work::Adopt)), a job is
//! owned again, and whatever is left unclaimed is killed as it drops.

use super::{Actor, Arc, CallId, Child, Completion, InFlight, Json, Msg, Ordering, contain, lock};
use crate::handover::{Handed, Kind, Owner};

/// What a frozen session stopped, each group with whose it is.
pub(super) struct Frozen {
    stopped: Vec<(Owner, Box<dyn ext::Parting>)>,
    committed: bool,
}

impl Actor {
    /// The next message a thaw let through, while none is frozen.
    pub(super) fn thawed(&mut self) -> Option<Msg> {
        self.frozen.is_none().then(|| self.deferred.pop_front())?
    }

    /// Whether the session's work can go on in the daemon's next build: it
    /// is at rest but for calls gone on in the background, each of which,
    /// like every group of the user's work the session owns, a tool takes
    /// up again there. Helpers are stopped, and started again as needed.
    pub(super) fn handable(&self) -> Result<(), String> {
        let session = &self.live.session;
        let tools = self.registry.tools();
        if !self.core.at_rest_but_background() {
            return Err(format!("session {} is at work", self.live.id.short()));
        }
        let resumable = lock(&session.resumable);
        for detached in self.core.ledger().detached() {
            let resumes = tools.resumable(&detached.tool)
                && (resumable.iter()).any(|resumed| resumed.call == detached.call);
            if !resumes {
                return Err(format!(
                    "{} call {} goes on in no other build",
                    detached.tool, detached.call
                ));
            }
        }
        let jobs = lock(&session.jobs);
        let stays = (jobs.iter())
            .filter(|job| job.role == ext::Role::Work)
            .find(|job| {
                !job.tool
                    .as_deref()
                    .is_some_and(|tool| tools.resumable(tool))
            });
        if let Some(job) = stays {
            return Err(format!("job {} goes on in no other build", job.lease.id()));
        }
        Ok(())
    }

    /// Stops every group of the session's work, then freezes the session:
    /// see [`super::Live::freeze`]. A refusal stops nothing, as what it
    /// stopped so far goes on as it drops.
    pub(super) async fn freeze_handover(&mut self) -> Result<(), String> {
        if self.frozen.is_some() {
            return Err(format!("session {} is frozen", self.live.id.short()));
        }
        self.handable()?;
        let session = Arc::clone(&self.live.session);
        let calls: Vec<_> = (lock(&session.resumable).iter())
            .map(|resumed| {
                (
                    resumed.call,
                    Arc::clone(&resumed.process),
                    resumed.state.clone(),
                )
            })
            .collect();
        let jobs: Vec<_> = (lock(&session.jobs).iter())
            .filter(|job| job.role == ext::Role::Work)
            .map(|job| (job.call, job.tool.clone(), Arc::clone(&job.lease)))
            .collect();
        let mut stopped = Vec::new();
        for (call, process, state) in calls {
            let ledger = self.core.ledger();
            let Some(tool) = (ledger.detached().iter())
                .find(|d| d.call == call)
                .map(|d| d.tool.clone())
            else {
                continue;
            };
            let parting = process
                .hand_over()
                .await
                .map_err(|e| format!("call {call}: {e}"))?;
            let owner = self.owner(call, tool, Kind::Call { state });
            stopped.push((owner, parting));
        }
        for (call, tool, lease) in jobs {
            let tool = tool.ok_or_else(|| format!("job {} has no tool", lease.id()))?;
            let parting = lease
                .hand_over()
                .await
                .map_err(|e| format!("job {}: {e}", lease.id()))?;
            stopped.push((self.owner(call, tool, Kind::Job), parting));
        }
        self.frozen = Some(Frozen {
            stopped,
            committed: false,
        });
        Ok(())
    }

    fn owner(&self, call: CallId, tool: String, kind: Kind) -> Owner {
        Owner {
            session: self.live.id,
            call,
            tool,
            kind,
        }
    }

    /// Calls the handover off, unless it was committed: every group it
    /// stopped goes on as it drops.
    pub(super) fn thaw_handover(&mut self) {
        if self.frozen.as_ref().is_some_and(|frozen| !frozen.committed) {
            self.frozen = None;
        }
    }

    /// Gives every stopped group up for good. The session stays frozen,
    /// for the daemon to stop: what its groups would still report here is
    /// the next build's to report.
    pub(super) fn commit_handover(&mut self) -> Vec<(sys::handover::Given, Owner)> {
        let Some(frozen) = self.frozen.as_mut().filter(|frozen| !frozen.committed) else {
            return Vec::new();
        };
        frozen.committed = true;
        let session = &self.live.session;
        let mut given = Vec::new();
        for (owner, parting) in std::mem::take(&mut frozen.stopped) {
            lock(&session.handed).push(owner.call);
            if matches!(owner.kind, Kind::Call { .. }) {
                self.inflight.remove(&owner.call);
            }
            let ext::Given { group, handles } = parting.commit();
            match group.parse() {
                Ok(group) => given.push((sys::handover::Given { group, handles }, owner)),
                // Released, its handles closed: as a crash leaves it.
                Err(_) => unclaimed(self.live.id),
            }
        }
        for msg in std::mem::take(&mut self.deferred) {
            refused(msg);
        }
        given
    }

    /// Holds `msg` back while the session is frozen; once it committed,
    /// nothing is taken up any more, and a request is refused so its
    /// client sends it again to the newer build.
    pub(super) fn hold(&mut self, msg: Msg) {
        if self.frozen.as_ref().is_some_and(|frozen| frozen.committed) {
            refused(msg);
        } else {
            self.deferred.push_back(msg);
        }
    }

    /// Takes up what the daemon's previous build handed this session that
    /// is not a call the log went on with ([`Actor::adopt_tool`]): the
    /// groups calls left to it, owned again through the tools that started
    /// them. Anything else it was handed is not this session's to go on
    /// with, and is killed as it drops.
    pub(super) fn adopt_jobs(&self) {
        let handed = std::mem::take(&mut *lock(&self.live.session.adopting));
        let fence = self.live.session.fence.load(Ordering::SeqCst);
        for Handed { claimed, owner } in handed {
            let Kind::Job = owner.kind else {
                unclaimed(self.live.id);
                continue;
            };
            let Some(origin) = self.live.session.origin(owner.call, fence) else {
                unclaimed(self.live.id);
                continue;
            };
            let cx = self.live.session.cx(origin.clone(), &self.registry);
            let registry = Arc::clone(&self.registry);
            let session = Arc::clone(&self.live.session);
            self.live.session.shared.rt.spawn(Box::pin(async move {
                let Some(tool) = registry.tools().get(&owner.tool) else {
                    return unclaimed(session.id);
                };
                match tool.rehold(ext::Native::new(claimed), &cx).await {
                    Ok(lease) => {
                        let _ = session.mail.send(Msg::Reheld {
                            call: owner.call,
                            tool: owner.tool,
                            lease,
                            admission: origin.admission,
                        });
                    }
                    Err(_) => unclaimed(session.id),
                }
            }));
        }
    }

    /// Goes on with call `call`, gone on in the background, whose group
    /// the daemon's previous build handed over: its outcome is reported as
    /// the call's would have been there.
    pub(super) fn adopt_tool(&mut self, call: CallId, tool: String, input: Json) {
        let taken = lock(&self.live.session.adopting)
            .extract_if(.., |handed| {
                handed.owner.call == call && matches!(handed.owner.kind, Kind::Call { .. })
            })
            .find_map(|Handed { claimed, owner }| match owner.kind {
                Kind::Call { state } => Some((state, claimed)),
                Kind::Job => None,
            });
        let Some((state, claimed)) = taken else {
            return;
        };
        self.inflight.insert(call, InFlight::Tool(Child::None));
        let registry = Arc::clone(&self.registry);
        let fence = self.live.session.fence.load(Ordering::SeqCst);
        let origin = (self.live.session)
            .origin(call, fence)
            .expect("actor generation");
        origin.detach.set();
        let cx = (self.live.session)
            .cx(origin.clone(), &registry)
            .detachable(origin.detach.clone());
        let session = Arc::clone(&self.live.session);
        origin.spawn(&*self.live.session.shared.rt, async move {
            let claimed = ext::Native::new(claimed);
            let resumed = registry.tools().resume(&tool, &input, state, claimed, &cx);
            let called = contain(resumed).await;
            session.emit(fence, Completion::Tool { call, called });
        });
    }
}

/// Refuses what a client, a session or a call asked of a session that
/// handed its work over: each is answered, never dropped.
fn refused(msg: Msg) {
    match msg {
        Msg::Request { reply, .. } => reply.send(Err(crate::upgrade::DRAINING.to_owned())),
        Msg::Enqueue { reply, .. } => reply.send(Err(protocol::InboxRefusal::Interrupted)),
        Msg::Context {
            request:
                super::ContextRequest::Note {
                    done: Some(done), ..
                },
            ..
        } => done.send(Err(ext::MutationError::Revoked)),
        _ => {}
    }
}

/// A group handed over that nothing here goes on with: killed as its claim
/// drops.
fn unclaimed(session: protocol::SessionId) {
    ext_logging::emit(ext_logging::Fact::Fault {
        session: Some(session),
        operation: ext_logging::Operation::Handover,
        kind: ext_logging::Fault(std::io::ErrorKind::NotFound),
    });
}
