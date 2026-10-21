//! Admission freezes authority before any child is created.
use super::{
    Actor, Arc, Body, BoxFuture, CallId, Child, ContextRequest, Cx, Deserialize, Event, InFlight,
    Json, Live, Open, Outcome, Own, Registry, Serialize, SessionId, Shared, lock, oneshot, or,
};
use crate::store::Header;
use ext::{DepthBudget, SpawnRefusal};
use std::io;

#[derive(Clone)]
pub(crate) struct SpawnIntent {
    pub awaited: bool,
    pub admitted_at: std::time::SystemTime,
    pub call: CallId,
    pub request: SpawnAuthority,
    pub model: String,
    pub routing: Result<crate::model::FrozenRoute, String>,
    pub installed: Vec<protocol::Installed>,
    pub image: Vec<Json>,
    pub extensions: Vec<String>,
    pub settings: ext::Checkpoint,
    pub refusal: Option<SpawnRefusal>,
}

/// The admitted authority needs no request codec or unresolved runner machinery.
#[derive(Clone)]
pub(crate) struct SpawnAuthority {
    pub owner: String,
    pub purpose: String,
    pub prompt: String,
    pub prefix: Option<(SessionId, protocol::Seq)>,
    pub agent: String,
    pub role: Json,
    pub depth: DepthBudget,
    pub budget: ext::Budget,
}

impl From<ext::Spawn> for SpawnAuthority {
    fn from(request: ext::Spawn) -> Self {
        Self {
            owner: request.owner().into(),
            purpose: request.purpose,
            prompt: request.prompt,
            prefix: match request.context {
                ext::ContextSource::Fresh => None,
                ext::ContextSource::Prefix { session, seq } => Some((session, seq)),
            },
            agent: request.runner.agent,
            role: Json::encode(&request.runner.role).expect("model role encodes"),
            depth: request.depth,
            budget: request.budget,
        }
    }
}

impl Serialize for SpawnAuthority {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (
            &self.owner,
            &self.purpose,
            &self.prompt,
            &self.prefix,
            &self.agent,
            &self.role,
            self.depth,
            self.budget,
        )
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SpawnAuthority {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        let (owner, purpose, prompt, prefix, agent, role, depth, budget) =
            Deserialize::deserialize(decoder)?;
        Ok(Self {
            owner,
            purpose,
            prompt,
            prefix,
            agent,
            role,
            depth,
            budget,
        })
    }
}

impl SpawnIntent {
    pub(crate) fn into_json(self) -> Result<Json, serde_json::Error> {
        protocol::publication::SpawnImage {
            authority: Json::encode(&(
                self.awaited,
                self.admitted_at,
                self.call,
                self.request,
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
            request,
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
    pub(super) fn open_spawn(
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
                    let entries: Vec<_> = self
                        .live
                        .frames
                        .entries()
                        .chain(
                            self.unsynced
                                .iter()
                                .flat_map(|(entries, _)| entries.iter().cloned()),
                        )
                        .collect();
                    let watch = entries.iter().find_map(|e| match e.body {
                        Body::Watch { child } if child == link.child => Some(e.seq),
                        _ => None,
                    });
                    let outcome = watch.and_then(|watch| {
                        entries.iter().find_map(|e| match &e.body {
                            Body::Fired {
                                watch: fired,
                                outcome,
                            } if *fired == watch => Some(outcome.clone()),
                            _ => None,
                        })
                    });
                    if let Some(outcome) = outcome {
                        let refusal = SpawnIntent::from_json(&link.intent)
                            .ok()
                            .and_then(|i| i.refusal);
                        let value = refusal.map_or_else(
                            || Json::encode(&outcome).unwrap(),
                            |r| Json::encode(&Result::<Outcome, _>::Err(r)).unwrap(),
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
                        Json::encode(&Result::<Outcome, _>::Err(SpawnRefusal::Unavailable(
                            "the call already awaits a child".into(),
                        )))
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

        let frozen = self.freeze_spawn(call, request, awaited);
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
                        && i.request.owner == request.owner()
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
        inherit_settings: bool,
    ) -> Result<String, String> {
        let session = &self.live.session;
        let layers = &session.shared.models.settings;
        match role {
            ext::ModelRole::Named(role) => child
                .service::<dyn ext::ModelRoles>()
                .ok_or_else(|| format!("no model service resolves role {role}"))
                .and_then(|roles| roles.model(role, states, layers, &session.workspace)),
            role if inherit_settings => session.role_model(role),
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

    fn freeze_spawn(&self, call: CallId, mut request: ext::Spawn, awaited: bool) -> SpawnIntent {
        let allowance = lock(&self.live.session.spawn)
            .as_ref()
            .map_or(DepthBudget::new(u16::MAX), |i| i.request.depth);
        let depth = request.depth.intersect(allowance).descend();
        let mut refusal = depth.as_ref().err().cloned();
        request.depth = depth.unwrap_or(DepthBudget::new(0));
        let kind = self.registry.kind(&request.runner.agent);
        let (inherit, inherit_settings) = kind.unwrap_or_default();
        if kind.is_none() {
            refusal = Some(SpawnRefusal::Unavailable(format!(
                "unknown child agent {}",
                request.runner.agent
            )));
        }
        let child = self.registry.select(inherit, None);
        if let Err(error) = &child {
            refusal = Some(SpawnRefusal::Unavailable(error.to_string()));
        }
        let child = child.unwrap_or_default();
        if !child.agents().any(|name| name == request.runner.agent) {
            refusal = Some(SpawnRefusal::Unavailable(format!(
                "agent {} is not inheritable",
                request.runner.agent
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
        child.kind_settings(&request.runner.agent, &mut states);
        let settings = states.settings_image().unwrap_or_else(|error| {
            refusal = Some(SpawnRefusal::Unavailable(error));
            ext::Checkpoint::default()
        });
        let selected = self.child_model(&request.runner.role, &child, &states, inherit_settings);
        if matches!(request.runner.role, ext::ModelRole::Named(_))
            && let Err(error) = &selected
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
        let mut image = Vec::new();
        let installed = self.registry.installed();
        if let ext::ContextSource::Prefix { session, seq } = request.context {
            if session != self.live.id || seq.get() > self.ordered_seq {
                refusal = Some(SpawnRefusal::Unavailable(
                    "prefix is not committed in the owning conversation".into(),
                ));
            } else {
                let entries: Vec<_> = self
                    .live
                    .frames
                    .entries()
                    .chain(
                        self.unsynced
                            .iter()
                            .flat_map(|(entries, _)| entries.iter().cloned()),
                    )
                    .filter(|e| e.seq <= seq)
                    .collect();
                image = crate::transcript::context_image(&entries);
            }
        }
        SpawnIntent {
            awaited,
            admitted_at: std::time::SystemTime::now(),
            call,
            request: request.into(),
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

impl ext::Sessions for Own {
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
        request: ext::Detached<ext::Spawn>,
    ) -> BoxFuture<'a, Result<ext::ConversationHandle, SpawnRefusal>> {
        Box::pin(async move {
            let session = self.0.upgrade().ok_or(SpawnRefusal::Interrupted)?;
            if !self.1.current(&session) {
                return Err(SpawnRefusal::Interrupted);
            }
            let (reply, result) = oneshot::channel();
            self.1.send(
                &session,
                ContextRequest::Child {
                    request: request.0,
                    reply: SpawnReply::Detached(reply),
                },
            );
            result.await.unwrap_or(Err(SpawnRefusal::Interrupted))
        })
    }
    fn runner(&self) -> ext::Runner {
        let agent = self
            .0
            .upgrade()
            .and_then(|s| {
                lock(&s.spawn)
                    .as_ref()
                    .map(|i| i.request.agent.clone())
                    .or_else(|| s.shared.models.settings.agent(&s.workspace))
            })
            .unwrap_or_else(|| ext::LOOP.into());
        ext::Runner {
            agent,
            role: ext::ModelRole::Session,
        }
    }

    fn spawn<'a>(
        &'a self,
        _cx: &'a Cx,
        request: ext::Awaited<ext::Spawn>,
    ) -> BoxFuture<'a, Result<Outcome, SpawnRefusal>> {
        Box::pin(async move {
            let session = self.0.upgrade().ok_or(SpawnRefusal::Interrupted)?;
            if !self.1.current(&session) {
                return Err(SpawnRefusal::Interrupted);
            }
            let (reply, result) = oneshot::channel();
            self.1.send(
                &session,
                ContextRequest::Child {
                    request: request.0,
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
        for entry in self.frames.entries() {
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
        super::recorded_registry(shared, source, Some(&image.installed))?
    } else {
        match (source, open) {
            (Some(source), Open::Load) => super::load_registry(header, shared, source)?,
            (Some(source), Open::Create) => source.registry(),
            (None, _) => shared.registry.get(),
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
        let names: Vec<_> = registry.extensions().map(|(name, _)| name).collect();
        if names
            != image
                .extensions
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        {
            return Err(io::Error::other("frozen child registrations differ"));
        }
    }
    Ok((registry, image))
}
