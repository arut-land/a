//! Side requests: admission, replay, bounded execution, and committed results.

use super::{
    Actor, Arc, Body, BoxFuture, CallId, CallPorts, Cancel, ContextOrigin, ContextRequest, Cx,
    Event, Json, Model, Msg, Opened, Outcome, PendingSide, Phase, Replied, Session, SideAdmitted,
    Ticket, contain, first, lock, oneshot, replied, reply, unless,
};

pub(super) enum SideFact {
    Retry {
        by: String,
        attempt: u16,
        failure: Json,
        after_ms: u64,
    },
    Done {
        outcome: Outcome,
        failure: Option<Json>,
    },
}

impl Actor {
    // Successful settlement revokes mutation authority, but a background
    // observer may not have submitted its request yet. Interrupts revoke both.
    pub(super) async fn background_owner_succeeded(&mut self, origin: &ContextOrigin) -> bool {
        if origin.fence != self.live.session.fence.load(super::Ordering::SeqCst) {
            return false;
        }
        self.indexed().await.is_ok() && self.journal.index.succeeded(origin.id)
    }

    pub(super) async fn admit_side(&mut self, ticket: Ticket, side: Option<CallId>) {
        if let Some(PendingSide {
            reply,
            thread,
            limited,
            cancel,
            origin,
            background,
            while_waiting,
        }) = self.sides.remove(&ticket)
        {
            if while_waiting
                && !self
                    .core
                    .ledger()
                    .call(origin.id)
                    .is_some_and(|o| matches!(o.phase, protocol::ledger::Phase::Waiting(_)))
            {
                cancel.set();
            }
            let succeeded = background && self.background_owner_succeeded(&origin).await;
            let side = side.filter(|side| {
                self.phase == Phase::Serving
                    && (!cancel.is_set() || while_waiting)
                    && (origin.current(&self.live.session)
                        || succeeded
                        || while_waiting && cancel.is_set())
                    && self.core.ledger().call(*side).is_some()
            });
            reply.send(side.map_or(Opened::Closed, |id| {
                self.inflight.insert(id, super::InFlight::Side);
                Opened::Fresh(SideAdmitted {
                    id,
                    thread,
                    limited,
                    cancel,
                })
            }));
        }
    }

    pub(super) fn side_fact(
        &mut self,
        side: CallId,
        fact: SideFact,
        done: oneshot::Sender<Result<(), String>>,
    ) -> Event {
        let ticket = self.ticket();
        self.replies.clients.insert(ticket, done);
        Event::Side(match fact {
            SideFact::Retry {
                by,
                attempt,
                failure,
                after_ms,
            } => kernel::SideEvent::Retry {
                side,
                by,
                attempt,
                failure,
                after_ms,
                ticket,
            },
            SideFact::Done { outcome, failure } => kernel::SideEvent::Done {
                side,
                outcome,
                failure,
                ticket,
            },
        })
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn open_side(
        &mut self,
        origin: ContextOrigin,
        registry: Arc<ext::host::Registry>,
        side: ext::ModelTurn,
        reply: oneshot::Sender<Opened>,
    ) -> Option<Event> {
        let of = origin.id;
        if side.is_while_waiting()
            && !self
                .core
                .ledger()
                .call(of)
                .is_some_and(|o| matches!(o.phase, protocol::ledger::Phase::Waiting(_)))
        {
            reply.send(Opened::Closed);
            return None;
        }
        self.indexed().await.ok()?;
        if let Some(id) = self.journal.index.side(&side.identity()) {
            let cached = if let Some(seq) = self.journal.index.result(id, self.live.frames.seq()) {
                match self.indexed_entry(seq).await.ok()?.body {
                    Body::ModelResult {
                        outcome, failure, ..
                    } => Some(Opened::Cached { outcome, failure }),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(cached) = cached {
                reply.send(cached);
            } else if let Some(waiters) = self.replies.invoked.get_mut(&id) {
                waiters.push(super::Waiter::Model(reply));
            } else {
                self.replies
                    .invoked
                    .insert(id, vec![super::Waiter::Model(reply)]);
            }
            return None;
        }
        if self.phase != Phase::Serving {
            reply.send(Opened::Closed);
            return None;
        }
        let prefix = match side.context {
            ext::SideContext::BeforeOwner => Some(of.0),
            ext::SideContext::None => None,
        };
        let turn = side.intent(prefix).ok()?;
        let identity = side.identity();
        let thread = if let Some(first) = self.journal.index.thread(&identity.thread) {
            let Body::ModelCall {
                request: Some(request),
                ..
            } = self.indexed_entry(first.0).await.ok()?.body
            else {
                return None;
            };
            let mut image = protocol::publication::ThreadImage::from_json(&request).ok()?;
            image.turn = turn;
            image
        } else {
            let spawn = ext::Spawn::from(&side);
            let frozen = self.freeze_spawn(of, spawn, false, &registry).await;
            protocol::publication::ThreadImage {
                turn,
                authority: frozen.into_json().ok()?,
            }
        };
        let model = super::spawn::SpawnIntent::from_json(&thread.authority)
            .ok()?
            .model;
        let request = thread.to_json().ok()?;
        let limited = self
            .core
            .ledger()
            .open_calls()
            .filter(|(_, o)| matches!(o.call, protocol::ledger::Call::Side { .. }))
            .count()
            >= 4;
        let ticket = self.ticket();
        let background = side.is_background();
        self.sides.insert(
            ticket,
            PendingSide {
                while_waiting: side.is_while_waiting(),
                background,
                cancel: Cancel::default(),
                origin,
                reply,
                thread,
                limited,
            },
        );
        Some(Event::Side(kernel::SideEvent::Open {
            of,
            purpose: side.thread.purpose,
            model,
            request,
            background,
            ticket,
        }))
    }
}

impl ext::Models for CallPorts {
    fn call<'a>(
        &'a self,
        _cx: &'a Cx,
        side: ext::ModelTurn,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        Box::pin(async move {
            let session = Arc::clone(&self.session);
            let (sent, answer) = oneshot::channel();
            let origin = self.origin.clone();
            if !origin.current(&session)
                && (!side.is_background()
                    || origin.fence != session.fence.load(super::Ordering::SeqCst))
            {
                return Err(llm::Error::incomplete());
            }
            let registry = Arc::clone(&self.registry);
            let runtime = Arc::clone(&session.shared.rt);
            runtime.spawn(Box::pin(async move {
                sent.send(side_call(&session, origin, side, &registry).await);
            }));
            answer
                .await
                .unwrap_or_else(|| Err(llm::Error::incomplete()))
        })
    }
}

async fn side_call(
    session: &Arc<Session>,
    origin: ContextOrigin,
    side: ext::ModelTurn,
    registry: &Arc<ext::host::Registry>,
) -> Result<llm::Response, llm::Error> {
    let invalid = |e: String| llm::Error::new(llm::Kind::InvalidRequest, e);
    let (asking, opened) = oneshot::channel();
    origin.send(
        session,
        ContextRequest::Side {
            registry: Arc::clone(registry),
            side: side.clone(),
            reply: asking,
        },
    );
    let opened = opened.await.ok_or_else(llm::Error::incomplete)?;
    let admitted = match opened {
        Opened::Cached { outcome, failure } => return side_answer(outcome, failure),
        Opened::Closed => return Err(llm::Error::incomplete()),
        Opened::Fresh(admitted) => admitted,
    };
    let id = admitted.id;
    let cancel = admitted.cancel.clone();
    let work = async {
        if admitted.limited {
            return Err(invalid("four side calls are already running".into()));
        }
        let Some(daemon) = session.shared.daemon.upgrade() else {
            return Ok((Outcome::Interrupted { output: None }, None));
        };
        let key = side.identity();
        let (owner, thread, cancel, registry) = (
            Arc::clone(session),
            admitted.thread.clone(),
            cancel.clone(),
            Arc::clone(registry),
        );
        rt::spawn(&*session.shared.rt, async move {
            Box::pin(daemon.model_turn(&owner, admitted.id, key, thread, &cancel, &registry)).await
        })
        .await
    };
    let result = first(work, session.shared.rt.sleep(side.budget.timeout)).await;
    if (result.is_none() || cancel.is_set())
        && let Some(daemon) = session.shared.daemon.upgrade()
    {
        cancel.set();
        daemon.interrupt(side.thread.conversation(session.id));
    }
    let (outcome, answer) = sided(session.id, result);
    let failure = answer.as_ref().err().and_then(|e| Json::encode(e).ok());
    let (done, committed) = oneshot::channel();
    let _ = session.mail.send(Msg::Completed {
        fence: origin.fence,
        completion: super::Completion::SideFact {
            side: id,
            fact: SideFact::Done { outcome, failure },
            done,
        },
    });
    match committed.await {
        Some(Ok(())) => answer,
        _ => Err(llm::Error::incomplete()),
    }
}

pub(super) async fn run_side(
    session: &Arc<Session>,
    cx: &Cx,
    side: &ext::ModelTurn,
    messages: llm::Messages,
    model: &Result<Model, llm::Error>,
    origin: &ContextOrigin,
) -> Replied {
    let (id, cancel, fence) = (origin.id, &origin.cancel, origin.fence);
    let start = session.shared.rt.now();
    let provider = cx.service::<dyn ext::SideProvider>();
    if provider.is_none()
        && let Err(error) = &model
    {
        return Replied::Failed(error.clone());
    }
    let mut request = llm::Request {
        messages,
        model: model.as_ref().map_or_default(Model::id),
        ..llm::Request::default()
    };
    request.generation.max_output_tokens = Some(side.budget.max_tokens);
    request.tool_choice = llm::ToolChoice::None;
    let mut attempt = 0u16;
    loop {
        let mut quiet = |_: &str| {};
        let result = contain(async {
            match (provider, &model) {
                (Some(provider), _) => replied(provider.reply(side, &request.messages).await),
                (None, Ok(model)) => reply(model, &request, cancel, &mut quiet).await,
                (None, Err(error)) => (Replied::Failed(error.clone()), None),
            }
        })
        .await
        .0;
        let after = match &result {
            Replied::Failed(error) => cx.service::<dyn ext::SideRetry>().and_then(|retry| {
                retry
                    .after(error, attempt, id.get())
                    .map(|after| (retry.name(), after))
            }),
            _ => None,
        };
        let Some((by, after)) = after else {
            return result;
        };
        if session
            .shared
            .rt
            .now()
            .saturating_duration_since(start)
            .saturating_add(after)
            >= side.budget.timeout
        {
            return result;
        }
        attempt = match attempt.checked_add(1) {
            Some(n) => n,
            None => return result,
        };
        let Replied::Failed(error) = &result else {
            return result;
        };
        let Ok(failure) = Json::encode(error) else {
            return result;
        };
        super::provider_error(session.id, error);
        let (done, committed) = oneshot::channel();
        let _ = session.mail.send(Msg::Completed {
            fence,
            completion: super::Completion::SideFact {
                side: id,
                fact: SideFact::Retry {
                    by: by.to_owned(),
                    attempt,
                    failure,
                    after_ms: u64::try_from(after.as_millis()).unwrap_or(u64::MAX),
                },
                done,
            },
        });
        if !matches!(committed.await, Some(Ok(()))) {
            return Replied::Interrupted(None);
        }
        session.shared.rt.sleep(after).await;
    }
}

fn side_answer(outcome: Outcome, failure: Option<Json>) -> Result<llm::Response, llm::Error> {
    if let Some(failure) = failure {
        return Err(llm::Error::from_json(failure.raw())
            .unwrap_or_else(|e| llm::Error::new(llm::Kind::Decode, e.to_string())));
    }
    match outcome {
        Outcome::Done { output } => llm::Response::from_json(output.raw())
            .map_err(|e| llm::Error::new(llm::Kind::Decode, e.to_string())),
        Outcome::Failed { error } => Err(llm::Error::new(llm::Kind::Other, error)),
        Outcome::Interrupted { .. } => Err(llm::Error::incomplete()),
    }
}

pub(super) fn sided(
    session: protocol::SessionId,
    result: Option<Result<(Outcome, Option<Json>), llm::Error>>,
) -> (Outcome, Result<llm::Response, llm::Error>) {
    let result = result.unwrap_or_else(|| {
        Err(llm::Error::new(
            llm::Kind::Transport,
            "the side request timed out",
        ))
    });
    let (outcome, answer) = match result {
        Ok((outcome, failure)) => {
            let answer = side_answer(outcome.clone(), failure);
            (outcome, answer)
        }
        Err(error) => (
            Outcome::Failed {
                error: error.to_string(),
            },
            Err(error),
        ),
    };
    if !matches!(outcome, Outcome::Interrupted { .. })
        && let Err(error) = &answer
    {
        super::provider_error(session, error);
    }
    (outcome, answer)
}

impl super::Daemon {
    async fn model_turn(
        &self,
        owner: &Arc<Session>,
        call: CallId,
        key: ext::SideIdentity,
        image: protocol::publication::ThreadImage,
        cancel: &Cancel,
        registry: &Arc<ext::host::Registry>,
    ) -> Result<(Outcome, Option<Json>), llm::Error> {
        let result = async {
            let authority = super::spawn::SpawnIntent::from_json(&image.authority)
                .map_err(std::io::Error::other)?;
            if let Some(refusal) = authority.refusal {
                return Err(std::io::Error::other(format!("{refusal:?}")));
            }
            let id = protocol::EnvelopeId {
                sender: owner.id,
                call,
                purpose: key.to_json().map_err(std::io::Error::other)?.raw().into(),
            };
            let header = crate::store::Header {
                id: key.thread.conversation(owner.id),
                workspace: owner.workspace.to_path_buf(),
                parent: Some(crate::store::Parent {
                    session: owner.id,
                    call: call.get(),
                }),
            };
            let live = self
                .owned_conversation(header, image.authority, Some(Arc::clone(registry)))
                .await?;
            if let Some(env) = lock(&owner.env).clone() {
                live.set_env(env)?;
            }
            if cancel.is_set() {
                return Ok((Outcome::Interrupted { output: None }, None));
            }
            live.exchange(
                protocol::Envelope {
                    id,
                    destination: live.id,
                    payload: image.turn.raw().into(),
                },
                cancel,
            )
            .await
        }
        .await;
        result.map_err(|error: std::io::Error| llm::Error::new(llm::Kind::Other, error.to_string()))
    }
}

impl Actor {
    pub(super) async fn prepare_thread(&mut self, call: CallId) {
        let prepared = async {
            let seq = protocol::Seq::new(self.core.ledger().turn_start())
                .ok_or("model-only turn has no admitted input")?;
            let mut entry = self.indexed_entry(seq).await.map_err(|e| e.to_string())?;
            if let Body::Take { input } = entry.body {
                entry = self.indexed_entry(input).await.map_err(|e| e.to_string())?;
            }
            let text = match &entry.body {
                Body::Enqueue { envelope } => envelope.payload.as_str(),
                Body::Input { text, .. } => text.as_str(),
                _ => return Err("model-only turn has no admitted input".into()),
            };
            let (turn, prefix) = ext::ModelTurn::from_intent(text).unwrap_or_else(|_| {
                let spawn = lock(&self.live.session.spawn);
                let spawn = spawn.as_ref().expect("model-only authority");
                let mut turn = ext::ModelTurn::new(
                    ext::ThreadKey {
                        extension: spawn.request.owner.clone(),
                        purpose: spawn.request.purpose.clone(),
                        owner_scope: ext::OwnerScope::Session,
                    },
                    ext::TurnKey(call.get().to_string()),
                    spawn.request.runner.role().clone(),
                    ext::SideContext::None,
                    llm::Messages::default(),
                );
                turn.budget = spawn.request.budget;
                (turn, None)
            });
            let mut messages = self.conversation(call).await.map_err(|e| e.to_string())?;
            if let Some(seq) = prefix {
                let parent = self.live.parent.ok_or("model-only context has no owner")?;
                let shared = Arc::clone(&self.live.session.shared);
                let context = rt::unblock(&*self.live.session.shared.rt, move || {
                    shared
                        .owner_image(parent.session, seq.get() - 1)
                        .map(|image| image.project())
                })
                .await
                .map_err(|e| e.to_string())?;
                let history = messages.len().saturating_sub(turn.messages.len());
                messages = messages
                    .iter()
                    .take(history)
                    .chain(context.iter())
                    .chain(turn.messages.iter())
                    .cloned()
                    .collect();
            }
            let model = self
                .live
                .session
                .model(&self.registry)
                .map_err(|error| llm::Error::new(llm::Kind::InvalidRequest, error));
            Ok::<_, String>((turn, messages, model))
        }
        .await;
        self.inflight.insert(
            call,
            match prepared {
                Ok(prepared) => super::InFlight::Thread(Box::new(prepared)),
                Err(error) => super::InFlight::Prepared(Box::new(Err(error))),
            },
        );
        self.live.session.emit(
            self.live.session.fence.load(super::Ordering::SeqCst),
            super::Completion::Event(Event::Prepared {
                call,
                decisions: Vec::new(),
                answered: None,
            }),
        );
    }

    pub(super) fn send_thread(&mut self, call: CallId) {
        let Some(prepared) = self.inflight.remove(&call) else {
            return;
        };
        let session = Arc::clone(&self.live.session);
        let registry = Arc::clone(&self.registry);
        let handle = session
            .origin(call, session.fence.load(super::Ordering::SeqCst))
            .expect("actor generation");
        let cancel = handle.cancel.clone();
        let fence = handle.fence;
        let cx = session.cx(handle.clone(), &registry);
        let origin = handle.clone();
        self.inflight.insert(call, super::InFlight::Model);
        handle.spawn(&*Arc::clone(&session.shared.rt), async move {
            let result = match prepared {
                super::InFlight::Thread(prepared) => {
                    let (turn, messages, model) = *prepared;
                    let running = unless(
                        &cancel,
                        Box::pin(run_side(&session, &cx, &turn, messages, &model, &origin)),
                    );
                    first(running, session.shared.rt.sleep(turn.budget.timeout))
                        .await
                        .flatten()
                        .unwrap_or(Replied::Interrupted(None))
                }
                super::InFlight::Prepared(prepared) => Replied::Failed(llm::Error::new(
                    llm::Kind::InvalidRequest,
                    prepared.err().unwrap_or_default(),
                )),
                _ => return,
            };
            session.emit(
                fence,
                super::Completion::Replied {
                    call,
                    replied: result,
                    message: None,
                },
            );
        });
    }
}
