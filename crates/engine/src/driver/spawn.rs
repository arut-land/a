//! Admission freezes authority before any child is created.
use super::{
    Actor, Arc, Body, BoxFuture, CallId, CallPorts, Child, ContextRequest, Cx, Event, InFlight,
    Json, Live, Open, Registry, SessionId, Shared, lock, oneshot, or,
};
use crate::store::Header;
use ext::{DepthBudget, SpawnRefusal};
use std::io;

#[derive(Clone)]
pub(crate) struct SpawnIntent {
    pub awaited: bool,
    pub admitted_at: std::time::SystemTime,
    pub call: CallId,
    pub request: ext::Spawn,
    pub model: String,
    pub routing: Result<crate::model::FrozenRoute, String>,
    pub installed: Vec<protocol::Installed>,
    pub image: protocol::publication::ContextImage,
    pub extensions: Vec<String>,
    pub settings: ext::Checkpoint,
    pub refusal: Option<SpawnRefusal>,
}

impl SpawnIntent {
    pub(crate) fn into_json(self) -> Result<Json, serde_json::Error> {
        protocol::publication::SpawnImage {
            runner: if matches!(self.request.runner, ext::Runner::ModelOnly { .. }) {
                protocol::publication::RunnerKind::ModelOnly
            } else {
                protocol::publication::RunnerKind::Agent
            },
            authority: Json::encode(&(
                self.awaited,
                self.admitted_at,
                self.call,
                self.request.to_json()?,
                self.model,
                self.routing,
                self.installed,
                self.refusal,
                self.extensions,
                self.settings,
            ))?,
            image: self.image,
        }
        .to_json()
    }

    pub(crate) fn from_json(json: &Json) -> Result<Self, serde_json::Error> {
        let image = protocol::publication::SpawnImage::from_json(json)?;
        let (
            awaited,
            admitted_at,
            call,
            request,
            model,
            routing,
            installed,
            refusal,
            extensions,
            settings,
        ) = image.authority.parse()?;
        Ok(Self {
            awaited,
            admitted_at,
            call,
            request: ext::Spawn::from_json(&request)?,
            model,
            routing,
            installed,
            image: image.image,
            extensions,
            settings,
            refusal,
        })
    }
}

pub(super) enum SpawnReply {
    Awaited(oneshot::Sender<Json>),
    Detached(oneshot::Sender<Result<ext::ConversationHandle, SpawnRefusal>>),
}

impl Actor {
    pub(super) async fn open_spawn(
        &mut self,
        call: CallId,
        request: ext::Spawn,
        reply: SpawnReply,
    ) -> Option<Event> {
        let previous = self.recorded_spawn(call, &request);
        let awaited = matches!(reply, SpawnReply::Awaited(_));
        match reply {
            SpawnReply::Detached(reply) => {
                if let Some(link) = previous {
                    let refusal = SpawnIntent::from_json(&link.intent)
                        .ok()
                        .and_then(|i| i.refusal);
                    reply.send(refusal.map_or(
                        Ok(ext::ConversationHandle {
                            session: link.child,
                        }),
                        Err,
                    ));
                    return None;
                }
                // Filled with the admitted id below; this sender stays behind the commit barrier.
                self.detached.push((SessionId::new(), reply));
            }
            SpawnReply::Awaited(reply) => {
                if let Some(link) = &previous {
                    let outcome = async {
                        self.indexed().await?;
                        let Some(seq) = self.journal.index.fired(link.child) else {
                            return Ok(None);
                        };
                        match self.indexed_entry(seq).await?.body {
                            Body::Fired { outcome, .. } => Ok(Some(outcome)),
                            _ => Err(io::Error::other("invalid watch index")),
                        }
                    }
                    .await;
                    let outcome = match outcome {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            reply.send(
                                Json::encode(&Result::<protocol::ToolOutcome, _>::Err(
                                    SpawnRefusal::Unavailable(error.to_string()),
                                ))
                                .unwrap(),
                            );
                            return None;
                        }
                    };
                    if let Some(outcome) = outcome {
                        let refusal = SpawnIntent::from_json(&link.intent)
                            .ok()
                            .and_then(|i| i.refusal);
                        let value = refusal.map_or_else(
                            || Json::encode(&outcome).unwrap(),
                            |r| Json::encode(&Result::<protocol::ToolOutcome, _>::Err(r)).unwrap(),
                        );
                        reply.send(value);
                        return None;
                    }
                }
                let Some(InFlight::Tool(child)) = self.inflight.get_mut(&call) else {
                    return None;
                };
                if matches!(child, Child::Waiting(_)) {
                    reply.send(
                        Json::encode(&Result::<protocol::ToolOutcome, _>::Err(
                            SpawnRefusal::Unavailable("the call already awaits a child".into()),
                        ))
                        .unwrap(),
                    );
                    return None;
                }
                *child = Child::Waiting(reply);
                if previous.is_some() {
                    return None;
                }
            }
        }

        let frozen = self
            .freeze_spawn(call, request, awaited, &Arc::clone(&self.registry))
            .await;
        let input = frozen.request.prompt.clone();
        let intent = frozen.into_json().expect("spawn intent encodes");
        let child = SessionId::new();
        if !awaited {
            self.detached.last_mut().unwrap().0 = child;
        }
        Some(Event::Spawn {
            call,
            child,
            input,
            intent,
            wait: awaited,
        })
    }
    fn recorded_spawn(&self, call: CallId, request: &ext::Spawn) -> Option<protocol::ledger::Link> {
        self.core
            .ledger()
            .links()
            .iter()
            .find(|link| {
                SpawnIntent::from_json(&link.intent).is_ok_and(|i| {
                    i.call == call
                        && i.request.owner() == request.owner()
                        && i.request.purpose == request.purpose
                })
            })
            .cloned()
    }

    fn child_model(
        &self,
        role: &ext::ModelRole,
        child: &Registry,
        states: &ext::States,
    ) -> Result<String, String> {
        let session = &self.live.session;
        let layers = &session.shared.models.settings;
        match role {
            ext::ModelRole::Named(role) => child
                .service::<dyn ext::ModelRoles>()
                .ok_or_else(|| format!("no model service resolves role {role}"))
                .and_then(|roles| roles.model(role, states, layers, &session.workspace)),
            role => {
                let base = || match child.service::<dyn ext::ModelSpec>() {
                    Some(spec) => spec
                        .model(states, layers, &session.workspace)
                        .map(|(m, _)| m),
                    None => ext::Layers::model(layers, &session.workspace).map(|(m, _)| m),
                };
                if matches!(role, ext::ModelRole::Fast) {
                    layers
                        .fast_model(&session.workspace)
                        .and_then(|m| m.map_or_else(base, Ok))
                } else {
                    base()
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn freeze_spawn(
        &mut self,
        call: CallId,
        mut request: ext::Spawn,
        awaited: bool,
        registry: &Registry,
    ) -> SpawnIntent {
        let allowance = lock(&self.live.session.spawn)
            .as_ref()
            .map_or(DepthBudget::new(u16::MAX), |i| i.request.depth);
        let depth = request.depth.intersect(allowance).descend();
        let mut refusal = depth.as_ref().err().cloned();
        request.depth = depth.unwrap_or(DepthBudget::new(0));
        let agent = request.runner.agent();
        let kind = agent.and_then(|name| registry.kind(name));
        let (inherit, inherit_settings) = kind.unwrap_or_default();
        if agent.is_some() && kind.is_none() {
            refusal = Some(SpawnRefusal::Unavailable(format!(
                "unknown child agent {}",
                agent.unwrap_or_default()
            )));
        }
        let child = registry.select(inherit, None);
        if let Err(error) = &child {
            refusal = Some(SpawnRefusal::Unavailable(error.to_string()));
        }
        let child = child.unwrap_or_default();
        if agent.is_some_and(|agent| !child.agents().any(|name| name == agent)) {
            refusal = Some(SpawnRefusal::Unavailable(format!(
                "agent {} is not inheritable",
                agent.unwrap_or_default()
            )));
        }
        let extensions = child
            .extensions()
            .map(|(name, _)| name.to_string())
            .collect();
        let mut states = child.states();
        let session = &self.live.session;
        if inherit_settings
            && let Err(error) = states.inherit_settings(&lock(&session.state_view()))
        {
            refusal = Some(SpawnRefusal::Unavailable(error));
        }
        if let Some(agent) = agent {
            child.kind_settings(agent, &mut states);
        }
        let settings = states.settings_image().unwrap_or_else(|error| {
            refusal = Some(SpawnRefusal::Unavailable(error));
            ext::Checkpoint::default()
        });
        let selected = self.child_model(request.runner.role(), &child, &states);
        if let Err(error) = &selected
            && (matches!(request.runner.role(), ext::ModelRole::Named(_))
                || matches!(request.runner, ext::Runner::ModelOnly { .. })
                    && child.service::<dyn ext::SideProvider>().is_none())
        {
            refusal = Some(SpawnRefusal::Unavailable(error.clone()));
        }
        let model = selected.unwrap_or_else(|_| "none".into());
        let env = lock(&session.env).clone();
        let routing = session
            .shared
            .models
            .model(&session.workspace, env.as_deref(), &model)
            .map(|m| m.freeze());
        let mut image = protocol::publication::ContextImage::default();
        let installed = registry.installed();
        match request.placement {
            ext::Placement::Here => {}
            ext::Placement::Machine(_) => {
                refusal = Some(SpawnRefusal::Placement(request.placement));
            }
        }
        if let ext::Context::Owner = request.context {
            let seq = call.0;
            if seq.get() > self.journal.ordered_seq {
                refusal = Some(SpawnRefusal::Unavailable(
                    "prefix is not committed in the owning conversation".into(),
                ));
            } else {
                let entries = self.live.entries().await;
                if let Err(error) = &entries {
                    refusal = Some(SpawnRefusal::Unavailable(error.to_string()));
                }
                if let Ok(entries) = entries {
                    let pending: Vec<_> = self.pending_entries().collect();
                    image = rt::unblock_with(
                        &*self.live.session.shared.rt,
                        rt::BlockingClass::Cpu,
                        rt::STORAGE_BYTES,
                        move || {
                            let entries: Vec<_> = entries
                                .into_iter()
                                .chain(pending)
                                .filter(|e| e.seq <= seq)
                                .collect();
                            crate::transcript::context_image(&entries)
                        },
                    )
                    .await;
                }
            }
        }
        SpawnIntent {
            awaited,
            admitted_at: std::time::SystemTime::now(),
            call,
            request,
            model,
            routing,
            installed,
            image,
            extensions,
            settings,
            refusal,
        }
    }
}

impl ext::Conversations for CallPorts {
    fn kind(&self) -> String {
        let session = &self.session;
        lock(&session.spawn)
            .as_ref()
            .and_then(|i| i.request.runner.agent().map(str::to_owned))
            .or_else(|| session.shared.models.settings.agent(&session.workspace))
            .unwrap_or_else(|| ext::LOOP.into())
    }

    fn turn<'a>(
        &'a self,
        _cx: &'a Cx,
        turn: ext::ModelTurn,
    ) -> BoxFuture<'a, Result<llm::Response, llm::Error>> {
        CallPorts::turn(self, turn)
    }

    fn send<'a>(
        &'a self,
        _cx: &'a Cx,
        message: ext::SendMessage,
    ) -> BoxFuture<'a, Result<(), protocol::InboxRefusal>> {
        self.send_message(message)
    }

    fn detach<'a>(
        &'a self,
        _cx: &'a Cx,
        request: ext::Spawn,
    ) -> BoxFuture<'a, Result<ext::ConversationHandle, SpawnRefusal>> {
        Box::pin(async move {
            let session = &self.session;
            if !self.origin.current(session) {
                return Err(SpawnRefusal::Interrupted);
            }
            let (reply, result) = oneshot::channel();
            self.origin.send(
                session,
                ContextRequest::Child {
                    request,
                    reply: SpawnReply::Detached(reply),
                },
            );
            result.await.unwrap_or(Err(SpawnRefusal::Interrupted))
        })
    }

    fn task<'a>(
        &'a self,
        _cx: &'a Cx,
        request: ext::Spawn,
    ) -> BoxFuture<'a, Result<protocol::ToolOutcome, SpawnRefusal>> {
        Box::pin(async move {
            let session = &self.session;
            if !self.origin.current(session) {
                return Err(SpawnRefusal::Interrupted);
            }
            let (reply, result) = oneshot::channel();
            self.origin.send(
                session,
                ContextRequest::Child {
                    request,
                    reply: SpawnReply::Awaited(reply),
                },
            );
            let value = result.await.ok_or(SpawnRefusal::Interrupted)?;
            // Kernel answers are outcomes; admission refusals carry a typed result.
            value
                .parse()
                .map(Ok)
                .or_else(|_| value.parse())
                .unwrap_or(Err(SpawnRefusal::Interrupted))
        })
    }
}

impl Live {
    pub(crate) fn pending_child(&self, child: SessionId) -> bool {
        let mut watch = None;
        let Ok(entries) = self.frames.entries() else {
            return false;
        };
        for entry in entries {
            match entry.body {
                Body::Watch { child: watched } if watched == child => watch = Some(entry.seq),
                Body::Fired { watch: fired, .. } if Some(fired) == watch => return false,
                _ => {}
            }
        }
        watch.is_some()
    }

    pub(crate) fn bound(self: Arc<Self>, budget: ext::Budget) {
        let runtime = Arc::clone(&self.session.shared.rt);
        let clock = Arc::clone(&runtime);
        runtime.spawn(Box::pin(async move {
            let finished = async {
                self.session.changed.until(|| !self.has_work()).await;
                true
            };
            if !or(finished, async {
                clock.sleep(budget.timeout).await;
                false
            })
            .await
            {
                self.interrupt();
            }
        }));
    }
}

pub(super) fn restore(
    header: &Header,
    shared: &Shared,
    open: Open,
    source: Option<&Arc<dyn ext::host::Generations>>,
    current: Arc<Registry>,
) -> io::Result<(Arc<Registry>, Option<SpawnIntent>)> {
    let image = if matches!(open, Open::Load) && header.parent.is_some() {
        let entries = shared.store.prefix(header.id, 1)?;
        let Body::Spawned { intent } = &entries[0].body else {
            return Err(io::Error::other("child has no spawn authority"));
        };
        Some(SpawnIntent::from_json(intent).map_err(io::Error::other)?)
    } else {
        None
    };
    let registry = if let Some(image) = &image {
        if source.is_none() && current.installed() == image.installed {
            current
        } else {
            super::recorded_registry(shared, source, Some(&image.installed))?
        }
    } else {
        match (source, open) {
            (Some(source), Open::Load) => super::load_registry(header, shared, source)?,
            (_, Open::Create) | (None, _) => current,
        }
    };
    let registry = if header.parent.is_some() {
        Arc::new(
            registry
                .select(
                    ext::Inheritance::All,
                    image.as_ref().map(|i| i.extensions.as_slice()),
                )
                .map_err(io::Error::other)?,
        )
    } else {
        registry
    };
    if let Some(image) = &image {
        registry
            .states()
            .restore_settings(&image.settings)
            .map_err(io::Error::other)?;
        if !registry
            .extensions()
            .map(|(name, _)| name)
            .eq(image.extensions.iter().map(String::as_str))
        {
            return Err(io::Error::other("frozen child registrations differ"));
        }
    }
    Ok((registry, image))
}

#[cfg(test)]
#[path = "spawn_context_tests.rs"]
mod context_tests;
