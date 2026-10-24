//! A stand-in engine for an extension's tests: calls run against a
//! registry, with state folded and processes recorded in memory.

use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use protocol::{Body, CallId, Entry, Environment, Epoch, Json, Outcome, Seq, SessionId};
use serde_json::Value;

use crate::host::{Host, Parts, Registry, SessionServices};
use crate::{BoxFuture, Called, Cancel, Cx, Gate, Gating, States};

/// The runtime the calls run on, as the engine provides it.
static RT: LazyLock<rt::Rt> = LazyLock::new(|| Arc::new(rt::Threads::default()));

/// Shared independently of call authority, as in the engine.
static SERVICES: LazyLock<Arc<SessionServices>> = LazyLock::new(|| {
    Arc::new(
        SessionServices::default()
            .with::<dyn rt::Runtime>(Arc::clone(&RT))
            .with::<dyn transport::HttpClients>(Arc::new(transport::Clients::new(Arc::clone(&RT)))),
    )
});

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct Rig {
    pub dir: PathBuf,
    pub env: Option<Environment>,
    pub cancel: Cancel,
    pub session: SessionId,
    host: Arc<Recorder>,
    calls: AtomicU64,
    _directory: tempfile::TempDir,
}

/// What the calls tell the engine.
pub struct Recorder {
    pub registry: Registry,
    states: Mutex<States>,
    /// The `ext` entries the calls logged.
    pub log: Mutex<Vec<Entry>>,
    pub spawned: Mutex<Vec<(u32, bool)>>,
    processes: Mutex<Vec<(Arc<dyn crate::RunningProcess>, crate::Flag)>>,
    /// The output the calls reported as they ran.
    pub progress: Mutex<String>,
    /// Notified each time a group the session owns ends.
    pub ends: Arc<rt::Notify>,
}

impl Deref for Rig {
    type Target = Recorder;

    fn deref(&self) -> &Recorder {
        &self.host
    }
}

impl Rig {
    /// A rig in a fresh directory named for `name`, its workspace.
    pub fn new(name: &str, registry: Registry) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(&format!("a-rig-{name}-"))
            .tempdir()
            .unwrap();
        let dir = directory.path();
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        Self {
            dir,
            env: None,
            cancel: Cancel::default(),
            session: SessionId::new(),
            host: Arc::new(Recorder {
                states: Mutex::new(registry.states()),
                registry,
                log: Mutex::default(),
                spawned: Mutex::default(),
                processes: Mutex::default(),
                progress: Mutex::default(),
                ends: Arc::default(),
            }),
            calls: AtomicU64::new(0),
            _directory: directory,
        }
    }

    pub fn workspace(&self) -> PathBuf {
        self.dir.join("ws")
    }

    pub fn spill(&self) -> PathBuf {
        self.dir.join("spill")
    }

    /// What the next call runs with.
    pub fn cx(&self) -> Cx {
        Parts {
            workspace: self.workspace().into(),
            env: self.env.clone().map(Arc::new),
            session: self.session,
            call: CallId::new(self.calls.fetch_add(1, Ordering::SeqCst) + 1).expect("from 1"),
            model: "test:model".into(),
            spill: self.spill().into(),
            cancel: self.cancel.clone(),
            services: Arc::clone(&SERVICES),
            detach: crate::Flag::default(),
        }
        .cx(Arc::clone(&self.host) as _)
    }

    /// Runs tool `tool` on `input`.
    pub fn call(&self, tool: &str, input: &Value) -> Outcome {
        let cx = self.cx();
        let input = Json::encode(input).expect("JSON");
        rt::block_on(self.registry.tools().run(tool, &input, None, &cx)).outcome
    }

    /// The output of a call that must succeed.
    pub fn done(&self, tool: &str, input: &Value) -> Value {
        match self.call(tool, input) {
            Outcome::Done { output } => output.parse().expect("a JSON output"),
            other => panic!("{tool} {input}: {other:?}"),
        }
    }

    /// As after a restart: state rebuilt from the log alone.
    pub fn restart(&self) {
        let mut states = self.registry.states();
        states.replay(lock(&self.log).iter());
        *lock(&self.states) = states;
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        for (process, released) in lock(&self.processes).drain(..) {
            let _ = process.signal(crate::ProcessSignal::Kill);
            rt::block_on(released.wait());
        }
    }
}

impl Recorder {
    fn append(&self, body: Body) -> crate::MutationResult {
        let mut log = lock(&self.log);
        let seq = Seq::new(log.last().map_or(1, |entry| entry.seq.get() + 1)).unwrap();
        let entry = Entry::new(seq, Epoch(0), body);
        let mut states = lock(&self.states);
        states.apply(&entry);
        let result = states.mutation_result(&entry);
        log.push(entry);
        result
    }
}

impl Host for Recorder {
    fn invoke(
        self: Arc<Self>,
        cx: Cx,
        name: String,
        tool: String,
        input: Json,
    ) -> BoxFuture<'static, Called> {
        Box::pin(async move {
            let call = CallId(
                self.append(Body::ToolCall {
                    invoke: Some(protocol::Invocation {
                        parent: cx.call,
                        name,
                    }),
                    tool: tool.clone(),
                    input: input.clone(),
                    id: None,
                    card: None,
                    by: protocol::Audience::Model,
                })
                .expect("in-memory intent"),
            );
            let context = cx.invocation().clone();
            let child = Parts {
                workspace: cx.workspace,
                env: cx.env,
                session: cx.session,
                call,
                model: cx.model,
                spill: cx.spill,
                cancel: cx.cancel,
                services: Arc::clone(&SERVICES),
                detach: crate::Flag::default(),
            }
            .cx(self.clone())
            .with_invocation(context);
            let gated = match self.registry.hooks().gate(&child.point(), &tool, &input) {
                Gating::Now(gated) => gated,
                Gating::Approving(_) => {
                    return Called::failed("test invocation needs approval".into());
                }
            };
            for (by, d) in gated.decisions {
                self.append(Body::Decision {
                    by: by.to_string(),
                    call: Some(call),
                    d,
                })
                .unwrap();
            }
            let mut called = match gated.gate {
                Gate::Run => {
                    self.registry
                        .tools()
                        .run(&tool, &gated.input, None, &child)
                        .await
                }
                Gate::Refuse(error) => Called::failed(error),
                Gate::Ask => Called::failed("test invocation needs approval".into()),
            };
            called.outcome = self
                .registry
                .hooks()
                .post_tool(&child.point(), (&tool, &gated.input), called.outcome)
                .value;
            self.append(Body::ToolResult {
                call,
                outcome: called.outcome.clone(),
                card: called.card.clone(),
            })
            .unwrap();
            called
        })
    }

    fn spawned(&self, pgid: u32, background: bool) {
        lock(&self.spawned).push((pgid, background));
    }

    fn own(&self, process: crate::ProcessLease) {
        self.spawned(process.id(), true);
        let released = crate::Flag::default();
        lock(&self.processes).push((Arc::clone(&process), released.clone()));
        let ends = Arc::clone(&self.ends);
        RT.spawn(Box::pin(async move {
            process.ended().await;
            drop(process);
            released.set();
            ends.notify();
        }));
    }

    fn progress(&self, text: &str) {
        lock(&self.progress).push_str(text);
    }

    /// Committed at once: the fold is in memory.
    fn note(
        &self,
        ext: &str,
        state: &str,
        data: Json,
    ) -> BoxFuture<'static, crate::MutationResult> {
        Box::pin(std::future::ready(self.append(Body::Ext {
            ext: ext.to_owned(),
            state: state.to_owned(),
            data,
        })))
    }

    fn fact(&self, fact: protocol::ExtensionFact) -> BoxFuture<'static, crate::MutationResult> {
        Box::pin(std::future::ready(self.append(Body::Fact { fact })))
    }

    fn publish(
        &self,
        namespace: &str,
        key: Json,
        text: String,
    ) -> BoxFuture<'static, crate::MutationResult> {
        Box::pin(std::future::ready(self.append(Body::Context {
            text,
            publication: Some(protocol::publication::Publication {
                namespace: namespace.to_owned(),
                key,
            }),
        })))
    }

    fn edit(&self, target: Seq, upto: Option<Seq>, replace: Option<Json>) {
        self.append(Body::Edit {
            target,
            upto,
            replace,
        })
        .unwrap();
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.states));
    }

    fn registry(&self) -> &Registry {
        &self.registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_named_rigs_never_remove_each_others_workspaces() {
        let first = Rig::new("same-name", Registry::default());
        let file = first.workspace().join("retained");
        std::fs::write(&file, "first").unwrap();
        let second = Rig::new("same-name", Registry::default());
        let second_dir = second.dir.clone();
        assert_ne!(first.dir, second.dir);
        drop(second);
        assert!(!second_dir.exists());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "first");
    }
}
