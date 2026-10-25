//! Queries: what a client asks the daemon's extensions without touching a
//! log, such as the files of a workspace that match what is being typed.
//! A query is ephemeral and answered by id, never logged; by type it
//! cannot be anything else, since its [`QueryCx`] has no way to append,
//! publish or change state.
//!
//! Its shape is the type its `answer` returns, as a tool's is: a reply at
//! once, a future of one, or a [`Streamed`] answer whose partials go to
//! the client as updates before its reply. What it is asked about is its
//! [`Query::Scope`]: the daemon, a workspace or a session, each a type
//! holding what the query is given about it.

use std::future::Future;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;

use protocol::{Json, MachineId, Role, ScopeKind, SessionId};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::host::{Registry, SessionServices};
use crate::shape::{Next, Now, Streamed, Streams, Waits};
use crate::{BoxFuture, Cancel};

/// A service clients query: `r.query(FileSearch)`.
pub trait Query: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    /// What it answers about. A query is asked in its own scope or a
    /// narrower one that implies it: a workspace query, about a session,
    /// answers about the session's workspace.
    type Scope: Scope = Daemon;
    type Request: DeserializeOwned + Send;
    type Reply: Serialize + Send;

    /// Answers `request`. What an implementation names as its return type
    /// is its shape ([`Answered`]): a `Result<Self::Reply, String>`, a
    /// future of one (an `async fn`), or a [`Streamed`] answer whose
    /// partials are its updates.
    fn answer(&self, request: Self::Request, cx: &QueryCx<Self::Scope>) -> impl Send;
}

/// What a query is answered with. Only the engine makes one.
pub struct QueryCx<S> {
    /// What it is asked about.
    pub scope: S,
    /// The asking client's machine, as its hello claimed (the daemon's own
    /// when it named none). Unauthenticated: tell a client on the daemon's
    /// machine from one elsewhere with it, never decide what it may do.
    pub machine: MachineId,
    /// What the asking client may do.
    pub role: Role,
    /// Set once the client cancels it or hangs up: the engine stops
    /// waiting then, so this is only for work it hands off.
    pub cancel: Cancel,
    services: Arc<SessionServices>,
    registry: Arc<Registry>,
}

impl<S> QueryCx<S> {
    /// Service `S`, if an extension or the engine provides it.
    pub fn service<T: ?Sized + 'static>(&self) -> Option<&T> {
        self.registry
            .service::<T>()
            .or_else(|| self.services.get::<T>())
    }
}

/// What a query may be asked about: [`Daemon`], [`Workspace`] or
/// [`Session`].
pub trait Scope: Sized + Send + Sync + 'static + sealed::Sealed {
    const KIND: ScopeKind;
    #[doc(hidden)]
    fn of(at: &At) -> Option<Self>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Daemon {}
    impl Sealed for super::Workspace {}
    impl Sealed for super::Session {}
}

/// The daemon as a whole: what any scope implies.
pub struct Daemon;

/// A workspace, by its root.
pub struct Workspace {
    pub path: Arc<Path>,
}

/// A session, and the workspace it runs in.
pub struct Session {
    pub id: SessionId,
    pub workspace: Arc<Path>,
}

impl Scope for Daemon {
    const KIND: ScopeKind = ScopeKind::Daemon;
    fn of(_: &At) -> Option<Self> {
        Some(Self)
    }
}

impl Scope for Workspace {
    const KIND: ScopeKind = ScopeKind::Workspace;
    fn of(at: &At) -> Option<Self> {
        match at {
            At::Workspace(path) | At::Session(_, path) => Some(Self {
                path: Arc::clone(path),
            }),
            At::Daemon => None,
        }
    }
}

impl Scope for Session {
    const KIND: ScopeKind = ScopeKind::Session;
    fn of(at: &At) -> Option<Self> {
        match at {
            At::Session(id, workspace) => Some(Self {
                id: *id,
                workspace: Arc::clone(workspace),
            }),
            At::Daemon | At::Workspace(_) => None,
        }
    }
}

/// What a query was asked about, as the engine resolved it.
#[derive(Clone, Debug)]
pub enum At {
    Daemon,
    Workspace(Arc<Path>),
    Session(SessionId, Arc<Path>),
}

/// What the engine gives every query besides its scope.
pub struct Asked {
    pub machine: MachineId,
    pub role: Role,
    pub cancel: Cancel,
    pub services: Arc<SessionServices>,
    pub registry: Arc<Registry>,
}

/// Where a streamed answer's updates go, each as it is made. Sending one
/// waits until the client has room for it, and the answer is not polled
/// meanwhile: a query streams no faster than its client reads.
pub trait Sink: Send + Sync {
    fn send(&self, update: Json) -> BoxFuture<'_, ()>;
}

/// A query's [`Sink`].
pub type Updates<'a> = &'a dyn Sink;

/// What a query's `answer` may return, by shape `M`: the answer, run to
/// its reply, its updates sent.
pub trait Answered<R, M>: Send {
    #[doc(hidden)]
    fn run<'a>(self, updates: Updates<'a>) -> BoxFuture<'a, Result<R, String>>
    where
        Self: 'a;
}

impl<R: Send> Answered<R, Now> for Result<R, String> {
    fn run<'a>(self, _: Updates<'a>) -> BoxFuture<'a, Result<R, String>>
    where
        Self: 'a,
    {
        Box::pin(std::future::ready(self))
    }
}

impl<R, F> Answered<R, Waits> for F
where
    F: Future<Output = Result<R, String>> + Send,
{
    fn run<'a>(self, _: Updates<'a>) -> BoxFuture<'a, Result<R, String>>
    where
        Self: 'a,
    {
        Box::pin(self)
    }
}

impl<R, P, F> Answered<R, Streams<P>> for Streamed<P, F, R>
where
    R: Send,
    P: Serialize + Send,
    F: Future<Output = Result<R, String>> + Send,
{
    fn run<'a>(mut self, updates: Updates<'a>) -> BoxFuture<'a, Result<R, String>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            loop {
                match self.next().await {
                    Next::Partial(partial) => {
                        let update = Json::encode(&partial).map_err(|e| e.to_string())?;
                        updates.send(update).await;
                    }
                    Next::Done(reply) => return reply,
                }
            }
        })
    }
}

/// A query over JSON, so queries of every type fit one registry.
pub trait DynQuery: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn scope(&self) -> ScopeKind;
    /// Answers `body` about `at`, its updates sent to `updates`.
    fn answer<'a>(
        &'a self,
        body: &Json,
        at: &At,
        asked: Asked,
        updates: Updates<'a>,
    ) -> BoxFuture<'a, Result<Json, String>>;
}

/// A typed query as a [`DynQuery`], its shape `M` found from what its
/// `answer` returns.
pub(crate) struct Erased<Q, M>(pub Q, pub PhantomData<fn() -> M>);

impl<Q, M> DynQuery for Erased<Q, M>
where
    Q: Query,
    Q::answer(..): Answered<Q::Reply, M>,
{
    fn name(&self) -> &str {
        Q::NAME
    }

    fn description(&self) -> &str {
        Q::DESCRIPTION
    }

    fn scope(&self) -> ScopeKind {
        <Q::Scope as Scope>::KIND
    }

    fn answer<'a>(
        &'a self,
        body: &Json,
        at: &At,
        asked: Asked,
        updates: Updates<'a>,
    ) -> BoxFuture<'a, Result<Json, String>> {
        let request = match body.parse::<Q::Request>() {
            Ok(request) => request,
            Err(e) => return Box::pin(std::future::ready(Err(format!("invalid request: {e}")))),
        };
        let Some(scope) = Q::Scope::of(at) else {
            let kind = <Q::Scope as Scope>::KIND;
            let error = format!("{} answers about a {kind}", Q::NAME);
            return Box::pin(std::future::ready(Err(error)));
        };
        let cx = QueryCx {
            scope,
            machine: asked.machine,
            role: asked.role,
            cancel: asked.cancel,
            services: asked.services,
            registry: asked.registry,
        };
        Box::pin(async move {
            let reply = self.0.answer(request, &cx).run(updates).await?;
            Json::encode(&reply).map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests;
