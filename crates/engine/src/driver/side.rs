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
            messages,
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
                    messages,
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

    pub(super) async fn open_side(
        &mut self,
        origin: ContextOrigin,
        model: String,
        side: ext::Side,
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
        if let Some(id) = self.journal.index.side(&side.identity(of)) {
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
            } else if let Some(waiters) = self.side_waiters.get_mut(&id) {
                waiters.push(reply);
            } else {
                self.side_waiters.insert(id, vec![reply]);
            }
            return None;
        }
        if self.phase != Phase::Serving {
            reply.send(Opened::Closed);
            return None;
        }
        let conversation = match side.context {
            ext::SideContext::BeforeOwner => self.conversation(of).await.ok()?,
            ext::SideContext::None => llm::Messages::default(),
        };
        let Ok(request) = side.intent(&conversation) else {
            reply.send(Opened::Closed);
            return None;
        };
        let mut messages = conversation;
        for message in side.messages.iter() {
            messages.push(message.clone());
        }
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
                messages,
                limited,
            },
        );
        Some(Event::Side(kernel::SideEvent::Open {
            of,
            purpose: side.purpose,
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
        cx: &'a Cx,
        side: ext::Side,
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
            let cx = cx.clone();
            let registry = Arc::clone(&self.registry);
            let runtime = Arc::clone(&session.shared.rt);
            runtime.spawn(Box::pin(async move {
                sent.send(side_call(&session, &cx, origin, side, &registry).await);
            }));
            answer
                .await
                .unwrap_or_else(|| Err(llm::Error::incomplete()))
        })
    }
}

async fn side_call(
    session: &Arc<Session>,
    cx: &Cx,
    origin: ContextOrigin,
    side: ext::Side,
    registry: &ext::host::Registry,
) -> Result<llm::Response, llm::Error> {
    let invalid = |e: String| llm::Error::new(llm::Kind::InvalidRequest, e);
    let routed = session.role_model(&side.role, registry);
    let model = routed
        .and_then(|spec| {
            let env = lock(&session.env).clone();
            session
                .shared
                .models
                .model(&session.workspace, env.as_deref(), &spec)
        })
        .map_err(invalid);
    let (asking, opened) = oneshot::channel();
    origin.send(
        session,
        ContextRequest::Side {
            model: model
                .as_ref()
                .map_or_else(|_| "unresolved".into(), Model::name),
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
    let work: BoxFuture<'_, Replied> = Box::pin(run_side(
        session,
        cx,
        &side,
        admitted,
        &model,
        &cancel,
        origin.fence,
    ));
    let work = async {
        unless(&cancel, work)
            .await
            .unwrap_or(Replied::Interrupted(None))
    };
    let result = first(work, session.shared.rt.sleep(side.budget.timeout)).await;
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

async fn run_side(
    session: &Arc<Session>,
    cx: &Cx,
    side: &ext::Side,
    admitted: SideAdmitted,
    model: &Result<Model, llm::Error>,
    cancel: &Cancel,
    fence: u64,
) -> Replied {
    let SideAdmitted {
        id,
        messages,
        limited,
        ..
    } = admitted;
    let start = session.shared.rt.now();

    if limited {
        return Replied::Failed(llm::Error::new(
            llm::Kind::InvalidRequest,
            "four side calls are already running",
        ));
    }
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

/// What a side request's reply, or its timing out, is logged as and
/// answered with.
pub(super) fn sided(
    session: protocol::SessionId,
    replied: Option<Replied>,
) -> (Outcome, Result<llm::Response, llm::Error>) {
    let (outcome, answer) = match replied {
        Some(Replied::Done(output)) => {
            let answer = llm::Response::from_json(output.raw())
                .map_err(|e| llm::Error::new(llm::Kind::Decode, e.to_string()));
            (Outcome::Done { output }, answer)
        }
        Some(Replied::Failed(error)) => {
            let failed = Outcome::Failed {
                error: error.to_string(),
            };
            (failed, Err(error))
        }
        Some(Replied::Interrupted(output)) => {
            let error = llm::Error::new(llm::Kind::Incomplete, "the call ended first");
            return (Outcome::Interrupted { output }, Err(error));
        }
        None => {
            let error = llm::Error::new(llm::Kind::Transport, "the side request timed out");
            let failed = Outcome::Failed {
                error: error.to_string(),
            };
            (failed, Err(error))
        }
    };
    let answer = answer.inspect_err(|error| super::provider_error(session, error));
    (outcome, answer)
}
