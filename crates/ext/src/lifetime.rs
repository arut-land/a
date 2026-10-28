//! Demand-owned resources, independent of a session's call authority or log.

use std::any::Any;
use std::collections::BTreeMap;
use std::io;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use crate::{BoxFuture, Cancel, Flag};

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The sharing domain of a trusted declaration. Workspace paths come from the engine.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    User,
    Workspace(PathBuf),
    Session(protocol::SessionId),
}

/// An exact declaration identity, paired with its admitted sharing domain.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstanceKey<K> {
    pub identity: K,
    pub scope: Scope,
}

/// A live resource whose owner can await both failure and native cleanup.
pub trait ScopedResource: Send + Sync + 'static {
    /// Resolves on spontaneous exit, without polling or restarting the resource.
    fn ended(&self) -> BoxFuture<'_, ()>;
    /// Cancels the resource and resolves only after its native resources are reclaimed.
    fn close(&self) -> BoxFuture<'_, ()>;
}

/// Daemon-wide admission and completion, shared by heterogeneous resource registries.
pub struct Lifetimes {
    spawn: crate::host::Spawn,
    admission: Mutex<Admission>,
}

#[derive(Default)]
struct Admission {
    closed: bool,
    tasks: Vec<(Cancel, Flag)>,
}

impl Lifetimes {
    pub fn new(spawn: crate::host::Spawn) -> Self {
        Self {
            spawn,
            admission: Mutex::default(),
        }
    }

    fn admit(&self, cancel: Cancel, ended: Flag) -> io::Result<()> {
        let mut admission = lock(&self.admission);
        if admission.closed {
            return Err(io::ErrorKind::Interrupted.into());
        }
        admission.tasks.retain(|(_, ended)| !ended.is_set());
        admission.tasks.push((cancel, ended));
        Ok(())
    }

    /// Closes admission before cancelling any actor. Does not keep an idle daemon alive.
    pub fn close(&self) {
        let tasks = {
            let mut admission = lock(&self.admission);
            admission.closed = true;
            admission.tasks.clone()
        };
        for (cancel, _) in tasks {
            cancel.set();
        }
    }

    /// After closing admission, wait within the daemon's existing shutdown deadline.
    pub async fn ended(&self) {
        let tasks = lock(&self.admission).tasks.clone();
        for (_, ended) in tasks {
            ended.wait().await;
        }
    }
}

/// A weak registry. Constructing one starts no actor and performs no I/O.
/// `K` must include every launch, policy and principal input that distinguishes recipients;
/// it must not be a display label or a secret-derived public identifier.
pub struct Scoped<K, T> {
    lifetimes: Arc<Lifetimes>,
    entries: Mutex<BTreeMap<InstanceKey<K>, Slot<T>>>,
}

struct Slot<T> {
    entry: Weak<Entry<T>>,
    demand: Weak<Demand<T>>,
}

struct Entry<T> {
    state: Mutex<State<T>>,
    ready: Flag,
    cancel: Cancel,
    ended: Flag,
}

enum State<T> {
    Dormant,
    Starting,
    Ready(Arc<T>),
    Failed(io::ErrorKind, String),
    Retiring,
}

struct Demand<T>(Arc<Entry<T>>);

impl<T> Drop for Demand<T> {
    fn drop(&mut self) {
        self.0.cancel.set();
    }
}

/// A request's demand. Clones and session retention all participate in last release.
pub struct ScopeLease<T> {
    value: Arc<T>,
    demand: Arc<Demand<T>>,
}

impl<T> Clone for ScopeLease<T> {
    fn clone(&self) -> Self {
        Self {
            value: Arc::clone(&self.value),
            demand: Arc::clone(&self.demand),
        }
    }
}

impl<T> Deref for ScopeLease<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> ScopeLease<T> {
    /// Explicitly retires this generation, including other admitted requests.
    /// Shared request cancellation instead drops only that request's lease.
    pub fn retire(&self) {
        self.demand.0.cancel.set();
    }

    pub fn cancelled(&self) -> &Cancel {
        &self.demand.0.cancel
    }

    /// Native cleanup, including when shutdown retires a still-leased resource.
    pub async fn retired(&self) {
        self.demand.0.ended.wait().await;
    }
}

impl<K: Ord + Clone, T: ScopedResource> Scoped<K, T> {
    pub fn new(lifetimes: Arc<Lifetimes>) -> Self {
        Self {
            lifetimes,
            entries: Mutex::default(),
        }
    }

    /// First demand starts one actor; concurrent callers join its startup. A successor
    /// waits for retirement, so two generations of one key never overlap native cleanup.
    /// Cancelling a waiter releases only its demand. Startup failures reach every waiter;
    /// only a subsequent explicit acquire may retry. `open` must observe its scope's
    /// cancellation and finish cleaning up any partial resource before returning an error.
    pub async fn acquire(
        &self,
        key: InstanceKey<K>,
        open: impl FnOnce(Cancel) -> BoxFuture<'static, io::Result<T>>,
    ) -> io::Result<ScopeLease<T>> {
        let mut open = Some(open);
        loop {
            let (entry, demand, start) = {
                let mut entries = lock(&self.entries);
                if lock(&self.lifetimes.admission).closed {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                if let Some(entry) = entries.get(&key).and_then(|slot| slot.entry.upgrade()) {
                    if entry.ended.is_set() {
                        entries.remove(&key);
                        continue;
                    }
                    let demand = if entry.cancel.is_set() {
                        None
                    } else {
                        entries.get(&key).and_then(|slot| slot.demand.upgrade())
                    };
                    (entry, demand, false)
                } else {
                    entries.retain(|_, slot| slot.entry.strong_count() > 0);
                    let entry = Arc::new(Entry {
                        state: Mutex::new(State::Dormant),
                        ready: Flag::default(),
                        cancel: Cancel::default(),
                        ended: Flag::default(),
                    });
                    self.lifetimes
                        .admit(entry.cancel.clone(), entry.ended.clone())?;
                    let demand = Arc::new(Demand(Arc::clone(&entry)));
                    entries.insert(
                        key.clone(),
                        Slot {
                            entry: Arc::downgrade(&entry),
                            demand: Arc::downgrade(&demand),
                        },
                    );
                    (entry, Some(demand), true)
                }
            };
            let Some(demand) = demand else {
                entry.ended.wait().await;
                continue;
            };
            if start {
                let actor = Arc::clone(&entry);
                let opening = open.take().expect("one startup per acquire")(entry.cancel.clone());
                (self.lifetimes.spawn)(Box::pin(async move {
                    *lock(&actor.state) = State::Starting;
                    match opening.await {
                        Ok(value) => {
                            let value = Arc::new(value);
                            *lock(&actor.state) = State::Ready(Arc::clone(&value));
                            actor.ready.set();
                            actor.cancel.until(value.ended()).await;
                            actor.cancel.set();
                            *lock(&actor.state) = State::Retiring;
                            value.close().await;
                        }
                        Err(error) => {
                            *lock(&actor.state) = State::Failed(error.kind(), error.to_string());
                            actor.ready.set();
                        }
                    }
                    actor.ended.set();
                }));
            }
            entry.ready.wait().await;
            let state = lock(&entry.state);
            return match &*state {
                State::Ready(value) if !entry.cancel.is_set() => Ok(ScopeLease {
                    value: Arc::clone(value),
                    demand,
                }),
                State::Failed(kind, text) => Err(io::Error::new(*kind, text.clone())),
                _ => Err(io::ErrorKind::Interrupted.into()),
            };
        }
    }
}

/// Only the live leases a session actually activated; never part of replayed state.
pub struct SessionLifetimes(Mutex<Option<Vec<Arc<dyn Any + Send + Sync>>>>);

impl Default for SessionLifetimes {
    fn default() -> Self {
        Self(Mutex::new(Some(Vec::new())))
    }
}

impl SessionLifetimes {
    /// Keeps demand through the session's next hibernation or retirement. Repeated
    /// requests for one generation do not accumulate retained leases.
    pub fn retain<T: ScopedResource>(&self, lease: &ScopeLease<T>) -> io::Result<()> {
        let demand: Arc<dyn Any + Send + Sync> = lease.demand.clone();
        let mut state = lock(&self.0);
        let held = state.as_mut().ok_or(io::ErrorKind::Interrupted)?;
        if !held.iter().any(|old| Arc::ptr_eq(old, &demand)) {
            held.push(demand);
        }
        Ok(())
    }

    /// Releases a retiring configuration generation while the session stays live.
    pub fn clear(&self) {
        let held = {
            let mut held = lock(&self.0);
            held.as_mut().map(std::mem::take)
        };
        drop(held);
    }

    pub fn close(&self) {
        let held = lock(&self.0).take();
        drop(held);
    }
}
