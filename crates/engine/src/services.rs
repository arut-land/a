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
            blobs: self.blobs.clone(),
            cancel: origin.cancel.clone(),
            services: Arc::clone(&self.services),
        }
        .cx(Arc::new(CallPorts {
            origin,
            session: Arc::clone(self),
            registry: Arc::clone(registry),
        }))
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
        .and_then(|i| i.request.runner.agent().map(str::to_owned))
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
    fn conversations(&self) -> Option<&dyn ext::Conversations> {
        Some(self)
    }

    fn invoke(
        self: Arc<Self>,
        cx: Cx,
        name: String,
        tool: String,
        input: Json,
    ) -> BoxFuture<'static, super::Called> {
        let (reply, result) = oneshot::channel();
        self.origin.send(
            &self.session,
            ContextRequest::Invoke {
                context: cx.invocation().clone(),
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
                held: Vec::new(),
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

    fn own(&self, process: ext::ProcessLease, role: ext::Role) {
        self.spawned(process.id(), true);
        self.origin
            .send(&self.session, ContextRequest::Own { process, role });
    }

    fn resumable(&self, process: Arc<dyn ext::RunningProcess>, state: Json) {
        let call = self.origin.id;
        let mut resumable = lock(&self.session.resumable);
        resumable.retain(|resumed| resumed.call != call);
        resumable.push(super::Resumed {
            call,
            process,
            state,
        });
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

    fn fact(&self, fact: protocol::ExtensionFact) -> BoxFuture<'static, ext::MutationResult> {
        self.mutation(Note::Fact(fact))
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

struct ModelOnlyBinding {
    registry: Arc<Registry>,
    model: String,
}
impl ext::host::Binding for ModelOnlyBinding {
    fn agent(&self) -> Box<dyn ext::Agent> {
        Box::new(ext::ModelOnly)
    }
    fn runner(&self) -> protocol::publication::RunnerKind {
        protocol::publication::RunnerKind::ModelOnly
    }
    fn installed(&self) -> Vec<protocol::Installed> {
        self.registry.installed()
    }
    fn callable(&self, _: &str) -> Option<ext::Callable> {
        None
    }
    fn model(&self) -> String {
        self.model.clone()
    }
}

pub(super) fn binding(
    bound: &Arc<Bound<Viewed>>,
    session: &Arc<Session>,
    registry: &Arc<Registry>,
) -> Arc<dyn ext::host::Binding> {
    if session.model_only() {
        Arc::new(ModelOnlyBinding {
            registry: Arc::clone(registry),
            model: session.model_spec(registry),
        })
    } else {
        Arc::clone(bound) as _
    }
}

impl Session {
    pub(super) fn model_only(&self) -> bool {
        lock(&self.spawn)
            .as_ref()
            .is_some_and(|i| matches!(i.request.runner, ext::Runner::ModelOnly { .. }))
    }
}

pub(super) struct SessionLogs(pub Arc<super::Shared>);

impl ext::Logs for SessionLogs {
    fn read(
        &self,
        root: SessionId,
        descendants: bool,
        cursors: Vec<(SessionId, ext::LogCursor)>,
    ) -> BoxFuture<'_, Result<Vec<ext::LogPrefix>, String>> {
        let shared = Arc::clone(&self.0);
        Box::pin(async move {
            rt::unblock(&*self.0.rt, move || {
                let records = shared.catalog.records();
                let mut ids = vec![root];
                if descendants {
                    loop {
                        let before = ids.len();
                        for record in &records {
                            if record
                                .header
                                .parent
                                .is_some_and(|p| ids.contains(&p.session))
                                && !ids.contains(&record.header.id)
                            {
                                ids.push(record.header.id);
                            }
                        }
                        if ids.len() == before {
                            break;
                        }
                    }
                }
                let namespace = shared
                    .registry
                    .get()
                    .service::<dyn ext::host::HistoryPolicy>()
                    .map(ext::host::HistoryPolicy::namespace);
                ids.into_iter()
                    .map(|session| {
                        let record = records
                            .iter()
                            .find(|r| r.header.id == session)
                            .ok_or("missing conversation")?;
                        let mut cursor = cursors
                            .iter()
                            .find(|(id, _)| *id == session)
                            .map_or_default(|(_, c)| *c);
                        let (reset, entries) = shared
                            .store
                            .suffix(session, &mut cursor, record.entries)
                            .map_err(|e| e.to_string())?;
                        let inherited_through = entries
                            .iter()
                            .rev()
                            .find_map(|e| match (&e.body, &namespace) {
                                (protocol::Body::Ext { ext, state, .. }, Some(n))
                                    if ext == &*n.ext && state == &*n.state =>
                                {
                                    Some(e.seq.get())
                                }
                                _ => None,
                            })
                            .unwrap_or(0);
                        Ok(ext::LogPrefix {
                            session,
                            entries,
                            cursor,
                            reset,
                            inherited_through,
                        })
                    })
                    .collect()
            })
            .await
        })
    }
}

impl Session {
    #[inline(never)]
    /// `ephemeral` is the role of a turn executed here for its owner,
    /// rather than by this session's own runner.
    pub(super) fn execution(
        &self,
        call: CallId,
        observed: super::Observation,
        agent: String,
        ephemeral: Option<&ext::ModelRole>,
    ) -> Option<Json> {
        let started = observed.started?;
        let (epoch, clock) = attempt_clock();
        let spawn = lock(&self.spawn);
        let runner = spawn.as_ref().map(|s| &s.request.runner);
        let side = ephemeral.is_some() || runner.is_some_and(|r| r.agent().is_none());
        let role = ephemeral.or(runner.map(ext::Runner::role));
        protocol::CallFact {
            id: protocol::AttemptId {
                executing_session: self.id,
                call,
                attempt: observed.attempt,
            },
            model: observed.model,
            role: role.unwrap_or(&ext::ModelRole::Session).to_string(),
            runner: if side { "model_only" } else { "agent" }.into(),
            agent,
            side,
            unix_ms: u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
            )
            .ok()?,
            clock_epoch: *epoch,
            start_ns: u64::try_from(started.saturating_duration_since(*clock).as_nanos()).ok()?,
            elapsed_ns: u64::try_from(
                self.shared
                    .rt
                    .now()
                    .saturating_duration_since(started)
                    .as_nanos(),
            )
            .ok(),
            usage: observed.usage,
            charge: None,
        }
        .to_json()
        .ok()
    }
}

pub(super) fn attempt_clock() -> &'static (SessionId, std::time::Instant) {
    static CLOCK: std::sync::OnceLock<(SessionId, std::time::Instant)> = std::sync::OnceLock::new();
    CLOCK.get_or_init(|| (SessionId::new(), std::time::Instant::now()))
}
