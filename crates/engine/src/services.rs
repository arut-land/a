//! Origin-bound extension ports resolve through their admitted registry.

use super::{
    Arc, Bound, BoxFuture, CallId, ContextOrigin, ContextRequest, Cx, Environment, Host, Json,
    Model, Note, Ordering, Parts, Path, Registry, Session, SessionId, States, View, Weak, lock,
    oneshot,
};

impl Session {
    pub(super) fn cx(self: &Arc<Self>, origin: ContextOrigin, registry: &Arc<Registry>) -> Cx {
        Parts {
            workspace: Arc::clone(&self.workspace),
            env: lock(&self.env).clone(),
            session: self.id,
            call: origin.id,
            model: self.model_spec(registry).into(),
            spill: Arc::clone(&self.spill),
            cancel: origin.cancel.clone(),
            services: Arc::clone(&self.services),
        }
        .cx(Arc::new(CallPorts {
            origin,
            session: Arc::clone(self),
            registry: Arc::clone(registry),
        }))
    }

    pub(super) fn role_model(
        &self,
        role: &ext::ModelRole,
        registry: &Registry,
    ) -> Result<String, String> {
        match role {
            ext::ModelRole::Session => self.model_named(registry).map(|(m, _)| m),
            ext::ModelRole::Fast => self
                .shared
                .models
                .settings
                .fast_model(&self.workspace)
                .and_then(|m| m.map_or_else(|| self.model_named(registry).map(|(m, _)| m), Ok)),
            ext::ModelRole::Named(role) => registry
                .service::<dyn ext::ModelRoles>()
                .ok_or_else(|| format!("no model service resolves role {role}"))?
                .model(
                    role,
                    &lock(&self.state_view()),
                    &self.shared.models.settings,
                    &self.workspace,
                ),
        }
    }

    pub(super) fn model(&self, registry: &Registry) -> Result<Model, String> {
        if let Some(image) = lock(&self.spawn).as_ref() {
            let route = image.routing.as_ref().map_err(Clone::clone)?;
            return self
                .shared
                .models
                .routed(&self.workspace, lock(&self.env).as_deref(), route);
        }
        let (spec, _) = self.model_named(registry)?;
        let env = lock(&self.env).clone();
        self.shared
            .models
            .model(&self.workspace, env.as_deref(), &spec)
    }

    pub(super) fn model_named(&self, registry: &Registry) -> Result<(String, String), String> {
        if let Some(image) = lock(&self.spawn).as_ref() {
            return Ok((image.model.clone(), "spawn intent".into()));
        }
        let layers = &self.shared.models.settings;
        match registry.service::<dyn ext::ModelSpec>() {
            Some(spec) => spec.model(&lock(&self.state_view()), layers, &self.workspace),
            None => ext::Layers::model(layers, &self.workspace),
        }
    }

    pub(super) fn model_spec(&self, registry: &Registry) -> String {
        self.model_named(registry)
            .map_or_else(|_| "none".to_owned(), |(model, _)| model)
    }
}

impl ext::Layers for crate::config::Settings {
    fn model(&self, workspace: &Path) -> Result<(String, String), String> {
        let (model, source) = self.model(workspace)?;
        Ok((model, source.to_string()))
    }
}

/// The session as handlers at decision points read it: only when one
/// asks, so a point with no handler costs nothing. A handler may take a
/// call's context along, to work it answers later with.
pub(crate) struct Viewed {
    session: Arc<Session>,
    registry: Arc<Registry>,
    fence: u64,
}

impl View for Viewed {
    fn session(&self) -> SessionId {
        self.session.id
    }

    fn workspace(&self) -> &Path {
        &self.session.workspace
    }

    /// As a call is recorded: the model's name once it is made, else
    /// what the settings asked for.
    fn model(&self) -> String {
        (self.session.model(&self.registry)).map_or_else(
            |_| self.session.model_spec(&self.registry),
            |model| model.name(),
        )
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.session.state_view()));
    }

    fn env(&self) -> Option<Arc<Environment>> {
        lock(&self.session.env).clone()
    }

    fn cx(&self, call: CallId) -> Option<Cx> {
        let origin = self.session.origin(call, self.fence)?;
        Some(self.session.cx(origin, &self.registry))
    }
}

/// `registry` bound to `session`: what the kernel drives.
pub(super) fn bind(registry: &Arc<Registry>, session: &Arc<Session>) -> Arc<Bound<Viewed>> {
    let settings = &session.shared.models.settings;
    let agent = lock(&session.spawn)
        .as_ref()
        .map(|i| i.request.agent.clone())
        .or_else(|| settings.agent(&session.workspace));
    let viewed = Arc::new(Viewed {
        session: Arc::clone(session),
        registry: Arc::clone(registry),
        fence: session.fence.load(Ordering::SeqCst),
    });
    Arc::new(Bound::new(Arc::clone(registry), viewed, agent))
}

pub(super) struct CallPorts {
    pub(super) origin: ContextOrigin,
    pub(super) session: Arc<Session>,
    pub(super) registry: Arc<Registry>,
}

impl CallPorts {
    fn report(&self, note: Note) {
        self.origin
            .send(&self.session, ContextRequest::Note { note, done: None });
    }
    fn mutation(&self, note: Note) -> BoxFuture<'static, ext::MutationResult> {
        let (done, committed) = oneshot::channel();
        self.origin.send(
            &self.session,
            ContextRequest::Note {
                note,
                done: Some(done),
            },
        );
        Box::pin(async move {
            committed
                .await
                .unwrap_or(Err(ext::MutationError::CommitFailed))
        })
    }
}

impl Host for CallPorts {
    fn models(&self) -> Option<&dyn ext::Models> {
        Some(self)
    }
    fn sessions(&self) -> Option<&dyn ext::Sessions> {
        Some(self)
    }

    fn invoke(
        self: Arc<Self>,
        _: Cx,
        name: String,
        tool: String,
        input: Json,
    ) -> BoxFuture<'static, super::Called> {
        let (reply, result) = oneshot::channel();
        self.origin.send(
            &self.session,
            ContextRequest::Invoke {
                name,
                tool,
                input,
                reply,
            },
        );
        Box::pin(async move {
            result.await.unwrap_or(super::Called {
                outcome: super::Outcome::Interrupted { output: None },
                card: None,
            })
        })
    }

    fn spawned(&self, pgid: u32, background: bool) {
        let Some(boot) = sys::boot_id() else { return };
        let boot = boot.to_owned();
        self.report(Note::Process {
            pgid,
            boot,
            background,
        });
    }

    fn own(&self, process: ext::OwnedProcess) {
        self.spawned(process.id(), true);
        self.origin
            .send(&self.session, ContextRequest::Own { process });
    }

    fn progress(&self, text: &str) {
        self.report(Note::Progress(text.to_owned()));
    }

    /// Resolves once the change is committed, so a read after it sees it.
    fn note(&self, ext: &str, state: &str, data: Json) -> BoxFuture<'static, ext::MutationResult> {
        self.mutation(Note::State {
            ext: ext.to_owned(),
            state: state.to_owned(),
            data,
        })
    }

    fn publish(
        &self,
        namespace: &str,
        key: Json,
        text: String,
    ) -> BoxFuture<'static, ext::MutationResult> {
        self.mutation(Note::Publish {
            namespace: namespace.to_owned(),
            key,
            text,
        })
    }

    fn edit(&self, target: protocol::Seq, upto: Option<protocol::Seq>, replace: Option<Json>) {
        self.report(Note::Edit {
            target,
            upto,
            replace,
        });
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.session.state_view()));
    }

    fn registry(&self) -> &Registry {
        &self.registry
    }
}

pub(super) struct SessionLayers(pub(super) Weak<Session>);

impl ext::Layers for SessionLayers {
    fn model(&self, workspace: &Path) -> Result<(String, String), String> {
        let session = self.0.upgrade().ok_or("the session is gone")?;
        ext::Layers::model(&session.shared.models.settings, workspace)
    }
}
