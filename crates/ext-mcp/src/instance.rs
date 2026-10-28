//! One framed reader and writer per process, with authority owned by each request.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use ext::{Cancel, Cx, Flag, OwnedProcess, ScopedResource};
use serde_json::{Value, json};

use crate::{Failure, incoming::Incoming, lock, rpc, stdio};

/// Typed startup results must fan out as values: Scoped's I/O errors retain only text.
pub(crate) struct Startup {
    pub outcome: Result<Instance, Failure>,
}

pub(crate) struct Lease(ext::ScopeLease<Startup>);

impl Lease {
    pub(crate) fn retain(
        scope: ext::ScopeLease<Startup>,
        lifetimes: &ext::SessionLifetimes,
    ) -> Result<Self, Failure> {
        scope.outcome.as_ref().map_err(Clone::clone)?;
        lifetimes.retain(&scope).map_err(|_| Failure::Cancelled)?;
        Ok(Self(scope))
    }
}

impl std::ops::Deref for Lease {
    type Target = Instance;

    fn deref(&self) -> &Instance {
        self.0
            .outcome
            .as_ref()
            .expect("only successful startup admits a lease")
    }
}

impl ScopedResource for Startup {
    fn ended(&self) -> ext::BoxFuture<'_, ()> {
        match &self.outcome {
            Ok(instance) => instance.ended(),
            // Keep the outcome until every joined caller has observed it and released demand.
            Err(_) => Box::pin(std::future::pending()),
        }
    }

    fn close(&self) -> ext::BoxFuture<'_, ()> {
        match &self.outcome {
            Ok(instance) => instance.close(),
            Err(_) => Box::pin(async {}),
        }
    }
}

pub(crate) struct Instance {
    child: Arc<OwnedProcess>,
    state: Arc<State>,
    finished: Flag,
    retired: Cancel,
    pub discovery: crate::discovery::Cache,
}

struct State {
    requests: Mutex<BTreeMap<rpc::RequestId, Pending>>,
    changed: rt::Notify,
    cancel: Cancel,
    scope: crate::config::Scope,
    budget: rt::Budget,
    discovery: crate::discovery::Cache,
    runtime: Arc<dyn rt::Runtime>,
}

struct Pending {
    frame: Option<Vec<u8>>,
    sent: bool,
    cancellation_sent: bool,
    incoming: Incoming,
    cx: Option<Cx>,
    reply: Option<rt::oneshot::Sender<Result<Value, Failure>>>,
    deadline: Instant,
    _credit: rt::Credit,
}

impl Instance {
    pub(crate) fn start(
        child: OwnedProcess,
        retired: Cancel,
        scope: crate::config::Scope,
        runtime: &Arc<dyn rt::Runtime>,
    ) -> Self {
        let child = Arc::new(child);
        let state = Arc::new(State {
            requests: Mutex::default(),
            changed: rt::Notify::new(),
            // Closing a failed handshake must not cancel the scope before its
            // typed startup outcome has reached every joined caller.
            cancel: Cancel::default(),
            scope,
            // Admission delays requests rather than rejecting a server's features.
            // Credit follows a cancelled exchange until its bounded drain finishes.
            budget: rt::Budget::new(32 * rpc::LIMIT),
            discovery: crate::discovery::Cache::default(),
            runtime: Arc::clone(runtime),
        });
        let finished = Flag::default();
        let done = finished.clone();
        let actor_state = Arc::clone(&state);
        let process = Arc::clone(&child);
        runtime.spawn(Box::pin(async move {
            let mut reader = stdio::Reader::default();
            let work = rt::or(
                actor_state.read(&process, &mut reader),
                rt::or(actor_state.write(&process), actor_state.maintenance()),
            );
            let failure = actor_state
                .cancel
                .until(work)
                .await
                .unwrap_or(Failure::Crashed);
            actor_state.cancel.set();
            let pending = std::mem::take(&mut *lock(&actor_state.requests));
            for (_, mut request) in pending {
                if let Some(reply) = request.reply.take() {
                    reply.send(Err(failure.clone()));
                }
            }
            done.set();
        }));
        Self {
            discovery: state.discovery.clone(),
            child,
            state,
            finished,
            retired,
        }
    }

    pub(crate) async fn discover(&self, cx: &Cx, deadline: Instant) -> Result<(), Failure> {
        let result = self
            .request("server/discover", json!({}), cx, deadline)
            .await?;
        if !result["supportedVersions"]
            .as_array()
            .is_some_and(|versions| versions.iter().any(|version| version == rpc::VERSION))
        {
            return Err(Failure::Protocol(
                "server does not support MCP 2026-07-28".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn revision(&self) -> u64 {
        self.state.discovery.revision()
    }

    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        cx: &Cx,
        deadline: Instant,
    ) -> Result<Value, Failure> {
        let runtime = cx.service::<dyn rt::Runtime>().expect("MCP runtime");
        let work = async {
            let credit = self.state.budget.acquire(rpc::LIMIT).await;
            if self.state.cancel.is_set() {
                return Err(Failure::Crashed);
            }
            let id = rpc::RequestId::new();
            let mut frame = serde_json::to_vec(&rpc::request(id, method, params))
                .map_err(|e| Failure::Protocol(e.to_string()))?;
            if frame.len() > rpc::LIMIT {
                return Err(Failure::Limit);
            }
            frame.push(b'\n');
            let (reply, result) = rt::oneshot::channel();
            lock(&self.state.requests).insert(
                id,
                Pending {
                    frame: Some(frame),
                    sent: false,
                    cancellation_sent: false,
                    incoming: Incoming::new(id),
                    cx: Some(cx.clone()),
                    reply: Some(reply),
                    deadline,
                    _credit: credit,
                },
            );
            let _waiting = Waiting {
                id,
                state: Arc::clone(&self.state),
            };
            self.state.changed.notify();
            result.await.unwrap_or(Err(Failure::Crashed))
        };
        rt::or(work, async {
            rt::or(
                async {
                    cx.cancel.wait().await;
                    Err(Failure::Cancelled)
                },
                async {
                    runtime
                        .sleep(deadline.saturating_duration_since(runtime.now()))
                        .await;
                    Err(Failure::Timeout)
                },
            )
            .await
        })
        .await
    }
}

impl ScopedResource for Instance {
    fn ended(&self) -> ext::BoxFuture<'_, ()> {
        Box::pin(async {
            rt::or(
                self.retired.wait(),
                rt::or(self.finished.wait(), self.child.process.ended()),
            )
            .await;
        })
    }
    fn close(&self) -> ext::BoxFuture<'_, ()> {
        Box::pin(async {
            self.state.cancel.set();
            self.child.close().await;
            self.finished.wait().await;
        })
    }
}

// A dropped caller releases its authority immediately. Shared exchanges keep only
// their charged ingress/drain state; they never retain a cancelled session's Cx.
struct Waiting {
    id: rpc::RequestId,
    state: Arc<State>,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        let mut requests = lock(&self.state.requests);
        let retire = !self.state.scope.shared() && requests.contains_key(&self.id);
        if let Some(request) = requests.get_mut(&self.id) {
            if request.frame.is_some() {
                requests.remove(&self.id);
            } else {
                request.cx = None;
                request.reply = None;
            }
        }
        drop(requests);
        if retire {
            self.state.cancel.set();
        }
        self.state.changed.notify();
    }
}

impl State {
    async fn write(&self, child: &OwnedProcess) -> Failure {
        let mut seen = self.changed.count();
        loop {
            let next = {
                let mut pending = lock(&self.requests);
                pending.iter_mut().find_map(|(&id, request)| {
                    if let Some(frame) = request.frame.take() { return Some((id, frame)); }
                    if request.sent && request.cx.is_none() && !request.cancellation_sent {
                        request.cancellation_sent = true;
                        let message = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id,"reason":"client stopped waiting"}});
                        return Some((id, format!("{message}\n").into_bytes()));
                    }
                    None
                })
            };
            let Some((id, bytes)) = next else {
                self.changed.changed(&mut seen).await;
                continue;
            };
            let input = child.input.as_ref().expect("stdio input");
            let mut remaining = bytes.as_slice();
            while !remaining.is_empty() {
                match input.write(remaining).await {
                    Ok(0) => return Failure::Crashed,
                    Ok(n) => remaining = &remaining[n..],
                    Err(e) => return stdio::io(&e),
                }
            }
            if let Err(e) = input.flush().await {
                return stdio::io(&e);
            }
            if let Some(request) = lock(&self.requests).get_mut(&id) {
                request.sent = true;
            }
            self.changed.notify();
        }
    }

    async fn read(&self, child: &OwnedProcess, reader: &mut stdio::Reader) -> Failure {
        loop {
            let (value, bytes) = match reader.read(child).await {
                Ok(frame) => frame,
                Err(e) => return e,
            };
            if value["method"] == "notifications/tools/list_changed" {
                if let Err(error) = rpc::decode(&value, None) {
                    return error;
                }
                self.discovery.invalidate();
            }
            if value.get("id").is_none()
                && let Err(error) = rpc::decode(&value, None)
            {
                return error;
            }
            let response_id = value.get("id").and_then(rpc::RequestId::read);
            let progress_id = rpc::RequestId::read(&value["params"]["progressToken"]);
            let mut requests = lock(&self.requests);
            if value.get("id").is_some() && response_id.is_none_or(|id| !requests.contains_key(&id))
            {
                return Failure::Protocol("response has no pending request".into());
            }
            let routed = response_id.or(progress_id.filter(|id| requests.contains_key(id)));
            let mut completed = None;
            for (&id, request) in requests.iter_mut() {
                if routed.is_some_and(|target| target != id) {
                    continue;
                }
                let result = request.incoming.decode(&value, bytes);
                let finished = !matches!(result, Ok(None));
                if let Some(cx) = &request.cx {
                    request.incoming.publish(cx, finished);
                }
                if finished {
                    let result = match result {
                        Ok(Some(result)) => Ok(result),
                        Err(error) => Err(error),
                        Ok(None) => unreachable!(),
                    };
                    completed = Some((id, result));
                    break;
                }
            }
            let completed = completed
                .and_then(|(id, result)| requests.remove(&id).map(|request| (request, result)));
            drop(requests);
            if let Some((mut request, result)) = completed {
                let corrupt = matches!(result, Err(Failure::Protocol(_) | Failure::Limit));
                if let Some(reply) = request.reply.take() {
                    reply.send(result);
                }
                if corrupt {
                    return Failure::Crashed;
                }
            }
            self.changed.notify();
        }
    }

    async fn maintenance(&self) -> Failure {
        let mut seen = self.changed.count();
        loop {
            let deadline = {
                let mut requests = lock(&self.requests);
                for request in requests.values_mut() {
                    if let Some(cx) = &request.cx {
                        request.incoming.publish(cx, false);
                    }
                }
                requests
                    .values()
                    .filter_map(|request| {
                        if request.cx.is_none() {
                            Some(request.deadline)
                        } else {
                            request.incoming.next_publication()
                        }
                    })
                    .min()
            };
            let Some(deadline) = deadline else {
                self.changed.changed(&mut seen).await;
                continue;
            };
            let now = self.runtime.now();
            if lock(&self.requests)
                .values()
                .any(|r| r.cx.is_none() && r.deadline <= now)
            {
                return Failure::Timeout;
            }
            rt::or(self.changed.changed(&mut seen), async {
                self.runtime
                    .sleep(deadline.saturating_duration_since(now))
                    .await;
            })
            .await;
        }
    }
}
