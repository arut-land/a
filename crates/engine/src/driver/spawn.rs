//! Admission freezes authority before any child is created.
use super::*;
use ext::{DepthBudget, SpawnRefusal};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SpawnIntent {
    pub call: CallId,
    pub request: ext::Spawn,
    pub model: String,
    pub installed: Vec<protocol::Installed>,
    pub image: Vec<Json>,
    pub refusal: Option<SpawnRefusal>,
}

pub(crate) fn spawn_image(entries: impl IntoIterator<Item = Entry>) -> Option<SpawnIntent> {
    entries.into_iter().find_map(|entry| match entry.body {
        Body::Spawned { intent } => intent.parse().ok(),
        _ => None,
    })
}

impl Actor {
    pub(super) fn open_spawn(
        &mut self,
        call: CallId,
        mut request: ext::Spawn,
        reply: oneshot::Sender<Json>,
    ) -> Option<Event> {
        let previous = self
            .core
            .ledger()
            .links()
            .iter()
            .find(|link| {
                link.intent.parse::<SpawnIntent>().is_ok_and(|i| {
                    i.call == call
                        && i.request.owner() == request.owner()
                        && i.request.purpose == request.purpose
                })
            })
            .cloned();
        let Some(InFlight::Tool(child)) = self.inflight.get_mut(&call) else {
            return None;
        };
        if let Child::Answered(value) = std::mem::replace(child, Child::Waiting(reply)) {
            let Child::Waiting(reply) = std::mem::replace(child, Child::None) else {
                unreachable!()
            };
            reply.send(value);
            return None;
        }
        if previous.is_some() {
            return None;
        }
        let allowance = lock(&self.live.session.spawn)
            .as_ref()
            .map_or(DepthBudget::new(u16::MAX), |i| i.request.depth);
        let depth = request.depth.intersect(allowance).descend();
        let mut refusal = depth.as_ref().err().cloned();
        request.depth = depth.unwrap_or(DepthBudget::new(0));
        match self.registry.inherited() {
            Err(e) => refusal = Some(SpawnRefusal::Unavailable(e.to_string())),
            Ok(registry) if !registry.agents().any(|a| a == request.runner.agent) => {
                refusal = Some(SpawnRefusal::Unavailable(format!(
                    "unknown child agent {}",
                    request.runner.agent
                )))
            }
            Ok(_) => {}
        }
        let session = &self.live.session;
        let model = match request.runner.role {
            ext::ModelRole::Session => session.model_named().map(|(m, _)| m),
            ext::ModelRole::Fast => session
                .shared
                .models
                .settings
                .fast_model(&session.workspace)
                .and_then(|m| m.map_or_else(|| session.model_named().map(|(m, _)| m), Ok)),
        }
        .unwrap_or_else(|_| "none".into());
        let input = request.prompt.clone();
        let intent = Json::encode(&SpawnIntent {
            call,
            request,
            model,
            installed: self.registry.installed(),
            image: Vec::new(),
            refusal,
        })
        .expect("spawn intent encodes");
        Some(Event::Spawn {
            call,
            child: SessionId::new(),
            input,
            intent,
            wait: true,
        })
    }
}

impl ext::Sessions for Own {
    fn runner(&self) -> ext::Runner {
        let agent = self
            .0
            .upgrade()
            .and_then(|s| {
                lock(&s.spawn)
                    .as_ref()
                    .map(|i| i.request.runner.agent.clone())
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
                    reply,
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
