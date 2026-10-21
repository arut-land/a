//! A stand-in engine for an extension's tests: calls run against a
//! registry, with state folded and processes recorded in memory.

use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use protocol::{Body, CallId, Entry, Environment, Epoch, Json, Outcome, Seq, SessionId};
use serde_json::Value;

use crate::host::{Host, Parts, Registry, Services};
use crate::{BoxFuture, Cancel, Cx, States};

/// The runtime the calls run on, as the engine provides it.
static RT: LazyLock<rt::Rt> = LazyLock::new(|| Arc::new(rt::Threads::default()));

/// The services the engine provides: its runtime.
static SERVICES: LazyLock<Arc<Services>> =
    LazyLock::new(|| Arc::new(Services::default().with::<dyn rt::Runtime>(Arc::clone(&RT))));

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
}

/// What the calls tell the engine.
pub struct Recorder {
    pub registry: Registry,
    states: Mutex<States>,
    /// The `ext` entries the calls logged.
    pub log: Mutex<Vec<Entry>>,
    pub spawned: Mutex<Vec<(u32, bool)>>,
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
        let dir = std::env::temp_dir().join(format!("a-rig-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
                progress: Mutex::default(),
                ends: Arc::default(),
            }),
            calls: AtomicU64::new(0),
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
        for &(pgid, _) in lock(&self.spawned).iter() {
            let _ = sys::Group(pgid).kill();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Host for Recorder {
    fn spawned(&self, pgid: u32, background: bool) {
        lock(&self.spawned).push((pgid, background));
    }

    fn own(&self, pgid: u32, ended: BoxFuture<'static, ()>) {
        self.spawned(pgid, true);
        let ends = Arc::clone(&self.ends);
        RT.spawn(Box::pin(async move {
            ended.await;
            ends.notify();
        }));
    }

    fn progress(&self, text: &str) {
        lock(&self.progress).push_str(text);
    }

    /// Committed at once: the fold is in memory.
    fn note(&self, ext: &str, state: &str, data: Json) -> BoxFuture<'static, ()> {
        lock(&self.states).fold(ext, state, &data);
        let mut log = lock(&self.log);
        let seq = log.len() as u64 + 1;
        log.push(Entry::new(
            Seq::new(seq).unwrap(),
            Epoch(0),
            Body::Ext {
                ext: ext.to_owned(),
                state: state.to_owned(),
                data,
            },
        ));
        Box::pin(std::future::ready(()))
    }

    fn publish(&self, ext: &str, state: &str, data: Json, text: String) -> BoxFuture<'static, ()> {
        {
            let mut log = lock(&self.log);
            let seq = Seq::new(log.len() as u64 + 1).unwrap();
            log.push(Entry::new(seq, Epoch(0), Body::Context { text }));
        }
        self.note(ext, state, data)
    }

    fn edit(&self, target: Seq, upto: Option<Seq>, replace: Option<Json>) {
        let mut log = lock(&self.log);
        let seq = log.len() as u64 + 1;
        let edit = Body::Edit {
            target,
            upto,
            replace,
        };
        log.push(Entry::new(Seq::new(seq).unwrap(), Epoch(0), edit));
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&lock(&self.states));
    }

    fn registry(&self) -> &Registry {
        &self.registry
    }
}
