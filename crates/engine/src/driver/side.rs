//! Side requests: admission, replay, bounded execution, and committed results.

use super::{
    Actor, Arc, Body, BoxFuture, CallId, CallPorts, Cancel, ContextOrigin, ContextRequest, Cx,
    Event, Json, Model, Msg, Opened, Outcome, PendingSide, Phase, Replied, Session, SideAdmitted,
    SideFact, Ticket, contain, first, lock, oneshot, replied, reply,
};
use protocol::Carried;

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
            ephemeral,
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
                let started = ephemeral.map(|role| {
                    let started = Arc::new(Started::default());
                    self.ephemeral.push((id, role, Arc::clone(&started)));
                    started
                });
                Opened::Fresh(SideAdmitted {
                    id,
                    thread,
                    limited,
                    cancel,
                    started,
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
        Event::Side(kernel::SideEvent::Fact { side, fact, ticket })
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
            self.await_reply(id, super::Waiter::Model(reply)).await;
            return None;
        }
        if self.phase != Phase::Serving {
            reply.send(Opened::Closed);
            return None;
        }
        let prefix = match side.context {
            ext::Context::Owner | ext::Context::OwnerPrefix => Some(of.0),
            ext::Context::Fresh => None,
        };
        let turn = side.intent(prefix).ok()?;
        let identity = side.identity();
        // An ephemeral turn has no thread to continue, so it freezes its own authority.
        let first = (!side.is_ephemeral())
            .then(|| self.journal.index.thread(&identity.thread))
            .flatten();
        let mut thread = if let Some(first) = first {
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
        let mut authority = super::spawn::SpawnIntent::from_json(&thread.authority).ok()?;
        authority.image = match side.context {
            ext::Context::Owner | ext::Context::OwnerPrefix => {
                let cut = self.context_cut(of.get() - 1).await.ok()?;
                let session = Arc::clone(&self.live.session);
                let registry = Arc::clone(&registry);
                rt::unblock(&*self.live.session.shared.rt, move || {
                    let presentation =
                        lock(&session.presentation).freeze(&registry, &*session.shared.blobs)?;
                    crate::context::with_presentation(cut, presentation, &*session.shared.blobs)
                })
                .await
                .ok()?
            }
            ext::Context::Fresh => protocol::publication::ContextImage::default(),
        };
        thread.authority = authority.into_json().ok()?;
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
                ephemeral: side.is_ephemeral().then(|| side.role.clone()),
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

impl CallPorts {
    pub(super) fn turn(
        &self,
        side: ext::ModelTurn,
    ) -> BoxFuture<'_, Result<llm::Response, llm::Error>> {
        Box::pin(async move {
            let session = &self.session;
            let (sent, answer) = oneshot::channel();
            let origin = self.origin.clone();
            if !origin.current(session)
                && (!side.is_background()
                    || origin.fence != session.fence.load(super::Ordering::SeqCst))
            {
                return Err(llm::Error::incomplete());
            }
            let session = Arc::clone(session);
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
    let ephemeral = side.is_ephemeral();
    let mut execution = None;
    let started = session.shared.rt.now();
    let work = async {
        if admitted.limited {
            return Err(invalid("four side calls are already running".into()));
        }
        if ephemeral {
            return ephemeral_turn(
                session,
                &origin,
                &admitted,
                &side,
                registry,
                started,
                &mut execution,
            )
            .await;
        }
        let Some(daemon) = session.shared.daemon.upgrade() else {
            return Ok((Outcome::Interrupted { output: None }, None));
        };
        let key = side.identity();
        let (owner, thread, cancel, registry) = (
            Arc::clone(session),
            admitted.thread,
            cancel.clone(),
            Arc::clone(registry),
        );
        rt::spawn(&*session.shared.rt, async move {
            Box::pin(daemon.model_turn(&owner, admitted.id, key, thread, &cancel, &registry)).await
        })
        .await
    };
    // An ephemeral turn races its own deadline, so a timeout still settles
    // the attempt it cut off.
    let result = if ephemeral {
        Some(work.await)
    } else {
        first(work, session.shared.rt.sleep(side.budget.timeout)).await
    };
    if (result.is_none() || cancel.is_set())
        && !ephemeral
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
            execution,
            done,
        },
    });
    match committed.await {
        Some(Ok(())) => answer,
        _ => Err(llm::Error::incomplete()),
    }
}

/// Where a model-only request's attempts are settled: the call they are
/// recorded under, what cancels them, the generation they belong to, and,
/// for an ephemeral turn, where the actor can read what each started.
#[derive(Clone, Copy)]
pub(super) struct Attempt<'a> {
    pub id: CallId,
    pub cancel: &'a Cancel,
    pub fence: u64,
    pub started: Option<&'a Started>,
}

/// The attempt an ephemeral turn has under way: when it began, its number
/// and model. An interrupt settles the turn in the owner's log before the
/// turn reports, so the actor settles the attempt from this instead.
#[derive(Default)]
pub(super) struct Started(pub(super) std::sync::Mutex<Option<(std::time::Instant, u16, String)>>);

pub(super) async fn run_side(
    session: &Arc<Session>,
    cx: &Cx,
    side: &ext::ModelTurn,
    messages: llm::Messages,
    model: &Result<Model, llm::Error>,
    at: Attempt<'_>,
    observed: &mut super::Observation,
) -> Replied {
    if side.context != ext::Context::OwnerPrefix {
        return run_side_once(session, cx, side, messages, model, at, observed).await;
    }
    // Preparation includes the registry instructions in this mandatory suffix.
    let prefix = &messages[..messages.len() - side.messages.len()];
    let mut summaries = String::new();
    let mut usage = llm::Usage::default();
    let mut last = None;
    for chunk in side.bounds().cover(prefix, &side.messages) {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                return Replied::Failed(llm::Error::new(llm::Kind::InvalidRequest, error));
            }
        };
        let result = run_side_once(session, cx, side, chunk, model, at, observed).await;
        if let Some(counts) = observed
            .usage
            .as_ref()
            .and_then(|json| llm::Usage::decode_counts(json).ok())
        {
            for (count, value) in llm::Count::ALL.into_iter().zip(counts) {
                let total = usage.count(count);
                if let Some(value) = value {
                    *total = Some(total.unwrap_or(0).saturating_add(value));
                }
            }
        }
        observed.usage = llm::Usage::encode_counts(&usage.counts()).ok();
        let Replied::Done(output) = result else {
            return result;
        };
        let response = match llm::Response::from_json(output.raw()) {
            Ok(response) => response,
            Err(error) => {
                return Replied::Failed(llm::Error::new(llm::Kind::Decode, error.to_string()));
            }
        };
        let text = response.message.text();
        if text.trim().is_empty() {
            return Replied::Failed(llm::Error::new(
                llm::Kind::Other,
                "the model wrote no summary",
            ));
        }
        if !summaries.is_empty() {
            summaries.push_str("\n\n");
        }
        summaries.push_str(&text);
        last = Some(response);
    }
    let Some(mut response) = last else {
        return Replied::Failed(llm::Error::new(llm::Kind::Other, "nothing to compact"));
    };
    response.message = llm::Assistant {
        content: vec![llm::AssistantPart::Text(llm::Text::new(summaries))],
        origin: None,
    };
    response.usage = usage;
    observed.usage = llm::Usage::encode_counts(&usage.counts()).ok();
    replied(Ok(response)).0
}

#[allow(clippy::too_many_lines)]
async fn run_side_once(
    session: &Arc<Session>,
    cx: &Cx,
    side: &ext::ModelTurn,
    messages: llm::Messages,
    model: &Result<Model, llm::Error>,
    at: Attempt<'_>,
    observed: &mut super::Observation,
) -> Replied {
    let Attempt {
        id,
        cancel,
        fence,
        started,
    } = at;
    let ephemeral = side.is_ephemeral().then_some(&side.role);
    let start = session.shared.rt.now();
    let provider = cx.service::<dyn ext::SideProvider>();
    if provider.is_none()
        && let Err(error) = &model
    {
        return Replied::Failed(error.clone());
    }
    let mut request = llm::Request {
        messages: messages.into(),
        model: model.as_ref().map_or_default(Model::id),
        ..llm::Request::default()
    };
    request.generation.max_output_tokens = Some(side.budget.max_tokens);
    request.tool_choice = llm::ToolChoice::None;
    let mut attempt = 0u16;
    loop {
        let name = model
            .as_ref()
            .map_or_else(|_| cx.model.to_string(), Model::name);
        *observed = super::Observation {
            attempt,
            model: name,
            ..super::Observation::default()
        };
        if let Some(started) = started {
            let _ = super::services::attempt_clock();
            *lock(&started.0) = Some((session.shared.rt.now(), attempt, observed.model.clone()));
        }
        let mut quiet = |_: &str| {};
        let result = contain(async {
            match (provider, &model) {
                (Some(provider), _) => {
                    let _ = super::services::attempt_clock();
                    observed.started = Some(session.shared.rt.now());
                    let response = match request.messages.canonical() {
                        Ok(messages) => provider.reply(side, &messages).await,
                        Err(error) => Err(error),
                    };
                    observed.usage = match &response {
                        Ok(response) => llm::Usage::encode_counts(&response.usage.counts()).ok(),
                        Err(error) => error.usage_json(),
                    };
                    replied(response)
                }
                (None, Ok(model)) => {
                    let preparing = model.clone();
                    let request = request.clone();
                    match rt::try_unblock_with(
                        &*session.shared.rt,
                        rt::BlockingClass::Cpu,
                        rt::STORAGE_BYTES,
                        move || preparing.prepare(&request),
                    )
                    .await
                    {
                        Ok(prepared) => reply(model, prepared, cancel, &mut quiet, observed).await,
                        Err(error) => (
                            Replied::Failed(llm::Error::new(llm::Kind::Other, error.to_string())),
                            None,
                        ),
                    }
                }
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
                execution: session.execution(
                    id,
                    std::mem::take(observed),
                    "model_only".into(),
                    ephemeral,
                ),
                done,
            },
        });
        if !matches!(committed.await, Some(Ok(()))) {
            return Replied::Interrupted(None);
        }
        session.shared.rt.sleep(after).await;
    }
}

/// An ephemeral turn runs here, under its owner's admission and bounds,
/// and leaves only the owner's result: no child log, no thread history.
async fn ephemeral_turn(
    session: &Arc<Session>,
    origin: &ContextOrigin,
    admitted: &SideAdmitted,
    side: &ext::ModelTurn,
    registry: &Arc<ext::host::Registry>,
    started: std::time::Instant,
    execution: &mut Option<Json>,
) -> Result<(Outcome, Option<Json>), llm::Error> {
    let mut side = side.clone();
    let (messages, model) = match prepare_ephemeral(session, admitted, &mut side, registry).await {
        Ok(prepared) => prepared,
        Err(error) => return Ok(failed(&error)),
    };
    let cx = session.cx(origin.clone(), registry);
    let mut observed = super::Observation::default();
    let at = Attempt {
        id: admitted.id,
        cancel: &admitted.cancel,
        fence: origin.fence,
        started: admitted.started.as_deref(),
    };
    let left = side
        .budget
        .timeout
        .saturating_sub(session.shared.rt.now().saturating_duration_since(started));
    let running = super::unless(
        &admitted.cancel,
        Box::pin(run_side(
            session,
            &cx,
            &side,
            messages,
            &model,
            at,
            &mut observed,
        )),
    );
    let replied = first(running, session.shared.rt.sleep(left)).await;
    let timed_out = replied.is_none();
    if timed_out {
        admitted.cancel.set();
    }
    let replied = replied.flatten().unwrap_or(Replied::Interrupted(None));
    if matches!(replied, Replied::Interrupted(_)) {
        observed.usage = None;
    }
    *execution = session.execution(admitted.id, observed, "model_only".into(), Some(&side.role));
    if timed_out {
        return Err(llm::Error::new(
            llm::Kind::Transport,
            "the side request timed out",
        ));
    }
    Ok(match replied {
        Replied::Done(output) => (Outcome::Done { output }, None),
        Replied::Failed(error) => failed(&error),
        Replied::Interrupted(_) => (Outcome::Interrupted { output: None }, None),
    })
}

async fn prepare_ephemeral(
    session: &Session,
    admitted: &SideAdmitted,
    side: &mut ext::ModelTurn,
    registry: &Arc<ext::host::Registry>,
) -> Result<(llm::Messages, Result<Model, llm::Error>), llm::Error> {
    let invalid = |e: String| llm::Error::new(llm::Kind::InvalidRequest, e);
    let authority = super::spawn::SpawnIntent::from_json(&admitted.thread.authority)
        .map_err(|e| invalid(e.to_string()))?;
    if let Some(refusal) = authority.refusal {
        return Err(invalid(format!("{refusal:?}")));
    }
    let child = registry
        .select(ext::Inheritance::All, Some(authority.extensions.as_slice()))
        .map_err(|e| invalid(e.to_string()))?;
    let context_mode = side.context;
    let image = authority.image;
    let registry = Arc::clone(registry);
    let places = session.blobs.places();
    let exchanges = (side.context != ext::Context::OwnerPrefix).then_some(side.bounds().exchanges);
    let context = rt::unblock(&*session.shared.rt, move || {
        if context_mode == ext::Context::Fresh {
            return Ok(llm::Messages::default());
        }
        super::Transcript::cut(
            &image,
            Arc::clone(&registry),
            registry.system_prompt(),
            &places,
            exchanges,
        )
        .map(super::Transcript::into_messages)
    })
    .await
    .map_err(|e| invalid(e.to_string()))?;
    let system = child.system_prompt();
    let current: llm::Messages = (!system.is_empty())
        .then(|| llm::Message::system(system))
        .into_iter()
        .chain(side.messages.iter().cloned())
        .collect();
    let messages = match side.context {
        ext::Context::OwnerPrefix => {
            side.messages = current;
            complete_prefix(context, &side.messages)
        }
        ext::Context::Fresh | ext::Context::Owner => side
            .bounds()
            .assemble(&[], &context, &current)
            .map_err(invalid)?,
    };
    let model = authority.routing.and_then(|route| {
        session
            .shared
            .models
            .routed(&session.workspace, lock(&session.env).as_deref(), &route)
    });
    Ok((messages, model.map_err(invalid)))
}

fn failed(error: &llm::Error) -> (Outcome, Option<Json>) {
    (
        Outcome::Failed {
            error: error.to_string(),
        },
        Json::encode(error).ok(),
    )
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

/// The input borrows its producer's presentation generation only while it is in flight.
struct InputRegistry<'a> {
    session: &'a Session,
    input: protocol::EnvelopeId,
}

impl Drop for InputRegistry<'_> {
    fn drop(&mut self) {
        lock(&self.session.input_registries).remove(&self.input);
    }
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
            let input = protocol::publication::ThreadInput {
                turn: image.turn,
                context: authority.image.clone(),
            };
            let held = input
                .context
                .root
                .iter()
                .cloned()
                .map(protocol::Held::Blob)
                .collect();
            let mut spawned = authority;
            spawned.image = protocol::publication::ContextImage::default();
            let live = self
                .owned_conversation(
                    header,
                    spawned.into_json().map_err(std::io::Error::other)?,
                    Some(Arc::clone(registry)),
                )
                .await?;
            if let Some(env) = lock(&owner.env).clone() {
                live.set_env(env)?;
            }
            if cancel.is_set() {
                return Ok((Outcome::Interrupted { output: None }, None));
            }
            lock(&live.session.input_registries).insert(id.clone(), Arc::clone(registry));
            let _registry = InputRegistry {
                session: &live.session,
                input: id.clone(),
            };
            live.exchange(
                protocol::Envelope::new(
                    id,
                    live.id,
                    Carried::Message(input.to_json().map_err(std::io::Error::other)?.raw().into()),
                    held,
                )
                .map_err(std::io::Error::other)?,
                cancel,
            )
            .await
        }
        .await;
        result.map_err(|error: std::io::Error| llm::Error::new(llm::Kind::Other, error.to_string()))
    }
}

impl Session {
    pub(super) async fn thread_context(
        self: &Arc<Self>,
        key: &protocol::EnvelopeId,
        context: protocol::publication::ContextImage,
        exchanges: Option<u16>,
    ) -> std::io::Result<llm::Messages> {
        let registry = lock(&self.input_registries).get(key).cloned();
        let owner = key.sender;
        let session = Arc::clone(self);
        rt::unblock(&*self.shared.rt, move || {
            let shared = &session.shared;
            let places =
                ext::blob::Blobs::new(Arc::clone(&shared.blobs), Arc::clone(&shared.tree), owner)
                    .places();
            let registry = match registry {
                Some(registry) => registry,
                None => shared.presentation_registry(&session.workspace, &context, &places)?,
            };
            super::Transcript::cut(
                &context,
                Arc::clone(&registry),
                registry.system_prompt(),
                &places,
                exchanges,
            )
            .map(super::Transcript::into_messages)
        })
        .await
    }
}

impl Actor {
    pub(super) async fn prepare_thread(&mut self, call: CallId) -> Result<super::InFlight, String> {
        let seq = self
            .core
            .ledger()
            .turn_start()
            .ok_or("model-only turn has no admitted input")?;
        let mut entry = self.indexed_entry(seq).await.map_err(|e| e.to_string())?;
        if let Body::Take { input, .. } = entry.body {
            entry = self.indexed_entry(input).await.map_err(|e| e.to_string())?;
        }
        let (text, key) = match &entry.body {
            Body::Enqueue { envelope } => match envelope.payload() {
                Carried::Message(text) => (text.clone(), Some(envelope.id().clone())),
                _ => return Err("model-only turn has no admitted input".into()),
            },
            Body::Input { segments, .. } => (segments.text(), None),
            _ => return Err("model-only turn has no admitted input".into()),
        };
        let text = text.as_str();
        let frozen = key.as_ref().and_then(|_| {
            let json = serde_json::from_str::<Json>(text).ok()?;
            let image = protocol::publication::ThreadInput::from_json(&json).ok()?;
            let (turn, prefix) = ext::ModelTurn::from_intent(image.turn.raw()).ok()?;
            Some((turn, image.context, prefix))
        });
        let (mut turn, context, prefix) = frozen
            .or_else(|| {
                let spawn = lock(&self.live.session.spawn);
                let spawn = spawn.as_ref()?;
                Some((
                    ext::ModelTurn::typed(&spawn.request, call, text)?,
                    protocol::publication::ContextImage::default(),
                    None,
                ))
            })
            .ok_or("model-only turn has no admitted authority")?;
        let policy = lock(&self.live.session.spawn)
            .as_ref()
            .and_then(|spawn| spawn.request.runner.context_policy())
            .ok_or("model-only context policy is missing")?;
        let history = match turn.context {
            ext::Context::OwnerPrefix => llm::Messages::default(),
            ext::Context::Fresh | ext::Context::Owner => self
                .bounded_history(CallId(seq), policy.exchanges)
                .await
                .map_err(|e| e.to_string())?,
        };
        let context = if let (Some(key), Some(_)) = (key.as_ref(), prefix) {
            self.live
                .session
                .thread_context(
                    key,
                    context,
                    (turn.context != ext::Context::OwnerPrefix).then_some(policy.exchanges),
                )
                .await
                .map_err(|e| e.to_string())?
        } else {
            llm::Messages::default()
        };
        let system = self.registry.system_prompt();
        let current: llm::Messages = (!system.is_empty())
            .then(|| llm::Message::system(system))
            .into_iter()
            .chain(turn.messages.iter().cloned())
            .collect();
        let messages = match turn.context {
            ext::Context::OwnerPrefix => {
                turn.messages = current;
                complete_prefix(context, &turn.messages)
            }
            ext::Context::Fresh | ext::Context::Owner => {
                policy.assemble(&history, &context, &current)?
            }
        };
        let model = self
            .live
            .session
            .model(&self.registry)
            .map_err(|error| llm::Error::new(llm::Kind::InvalidRequest, error));
        Ok(super::InFlight::Thread(Box::new((turn, messages, model))))
    }
}

fn complete_prefix(mut context: llm::Messages, current: &llm::Messages) -> llm::Messages {
    for message in current.iter().cloned() {
        context.push(message);
    }
    context
}
