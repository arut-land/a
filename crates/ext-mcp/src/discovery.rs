//! Discovery retains at most one response-sized aggregate. Output and its
//! escaped card therefore leave ample room in a 16 MiB protocol frame.
use std::io::{self, Write};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use ext::{Cancel, Cx, Flag};

use crate::Failure;
use serde_json::Value;

pub(crate) struct Budget(usize);
impl Default for Budget {
    fn default() -> Self {
        Self(crate::rpc::LIMIT)
    }
}
impl Budget {
    pub(crate) fn include(&mut self, value: &Value) -> Result<(), Failure> {
        serde_json::to_writer(self, value).map_err(|_| Failure::Limit)
    }
}
impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or(io::ErrorKind::FileTooLarge)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn description(text: &str) -> String {
    const BYTES: usize = 512;
    if text.len() <= BYTES {
        text.to_owned()
    } else {
        format!(
            "{}…",
            &text[..text.floor_char_boundary(BYTES - '…'.len_utf8())]
        )
    }
}

/// One validated list, with the earliest page's freshness deadline.
#[derive(Clone)]
pub(crate) struct Loaded {
    pub tools: Arc<[crate::RemoteTool]>,
    pub expires: Option<Instant>,
    pub authorization: Option<crate::oauth::GenerationId>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub authorization: Option<crate::oauth::GenerationId>,
    pub revision: u64,
}

#[derive(Clone, Default)]
pub(crate) struct Cache(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    revision: u64,
    uncached: bool,
    entry: Option<Box<Entry>>,
}
#[derive(Default)]
struct Entry {
    ready: Option<(Key, Loaded)>,
    flight: Option<(Key, Weak<Flight>, Weak<Demand>)>,
}
struct Flight {
    done: Flag,
    result: Mutex<Option<Result<Loaded, Failure>>>,
}
struct Demand(Cancel);
impl Drop for Demand {
    fn drop(&mut self) {
        self.0.set();
    }
}

impl Cache {
    pub(crate) fn revision(&self) -> u64 {
        crate::lock(&self.0).revision
    }

    pub(crate) fn invalidate(&self) {
        let mut state = crate::lock(&self.0);
        state.revision += 1;
        state.entry = None;
    }

    fn publish(&self, key: &Key, loaded: &Loaded, now: Instant) {
        let mut state = crate::lock(&self.0);
        if state.revision != key.revision {
            return;
        }
        state.uncached = loaded.expires.is_none_or(|expiry| now >= expiry);
        if state.uncached {
            state.entry = None;
            return;
        }
        let entry = state.entry.get_or_insert_with(Box::default);
        entry.ready = Some((
            Key {
                authorization: loaded.authorization.clone(),
                revision: key.revision,
            },
            loaded.clone(),
        ));
        entry.flight = None;
    }

    fn abandon(&self, done: &Arc<Flight>) {
        let mut state = crate::lock(&self.0);
        if state
            .entry
            .as_ref()
            .and_then(|entry| entry.flight.as_ref())
            .is_some_and(|(_, flight, _)| std::ptr::eq(flight.as_ptr(), Arc::as_ptr(done)))
        {
            state.entry = None;
        }
    }

    pub(crate) async fn get(
        &self,
        key: Key,
        cx: &Cx,
        fetch: impl FnOnce(Cx) -> ext::BoxFuture<'static, Result<Loaded, Failure>> + Send + 'static,
    ) -> Result<Arc<[crate::RemoteTool]>, Failure> {
        let runtime = cx.service_arc::<dyn rt::Runtime>().expect("MCP runtime");
        let selected = {
            let mut state = crate::lock(&self.0);
            if let Some((cached_key, loaded)) =
                state.entry.as_ref().and_then(|entry| entry.ready.as_ref())
                && cached_key.revision == key.revision
                && state.revision == key.revision
                && loaded.reusable(&key, runtime.now())
            {
                return Ok(Arc::clone(&loaded.tools));
            }
            if state.uncached {
                None
            } else {
                let entry = state.entry.get_or_insert_with(Box::default);
                entry.ready = None;
                let joining = entry
                    .flight
                    .as_ref()
                    .filter(|(current, _, _)| current == &key)
                    .and_then(|(_, flight, demand)| {
                        Some((
                            flight.upgrade().filter(|flight| !flight.done.is_set())?,
                            demand.upgrade()?,
                        ))
                    });
                if let Some((flight, demand)) = joining {
                    Some((flight, demand, false))
                } else {
                    let flight = Arc::new(Flight {
                        done: Flag::default(),
                        result: Mutex::default(),
                    });
                    let demand = Arc::new(Demand(Cancel::default()));
                    entry.flight = Some((
                        key.clone(),
                        Arc::downgrade(&flight),
                        Arc::downgrade(&demand),
                    ));
                    Some((flight, demand, true))
                }
            }
        };
        let Some((flight, demand, start)) = selected else {
            let loaded = fetch(cx.clone()).await?;
            self.publish(&key, &loaded, runtime.now());
            return Ok(loaded.tools);
        };
        let fetch = if start {
            let mut work_cx = cx.clone();
            work_cx.cancel = demand.0.clone();
            let cancel = demand.0.clone();
            let done = Arc::clone(&flight);
            let cache = self.clone();
            let work_key = key.clone();
            let clock = Arc::clone(&runtime);
            runtime.spawn(Box::pin(async move {
                let result = cancel
                    .until(fetch(work_cx))
                    .await
                    .unwrap_or(Err(Failure::Cancelled));
                if let Ok(loaded) = &result {
                    cache.publish(&work_key, loaded, clock.now());
                } else {
                    cache.abandon(&done);
                }
                *crate::lock(&done.result) = Some(result);
                done.done.set();
            }));
            None
        } else {
            Some(fetch)
        };
        let _demand = demand;
        rt::or(
            async {
                flight.done.wait().await;
                let loaded = crate::lock(&flight.result)
                    .as_ref()
                    .expect("completed discovery")
                    .clone()?;
                if start
                    || (self.revision() == key.revision && loaded.reusable(&key, runtime.now()))
                {
                    return Ok(loaded.tools);
                }
                // Joining unfinished work does not make its eventual result cacheable.
                let loaded =
                    fetch.expect("a follower retains its independent fetch")(cx.clone()).await?;
                self.publish(&key, &loaded, runtime.now());
                Ok(loaded.tools)
            },
            async {
                cx.cancel.wait().await;
                Err(Failure::Cancelled)
            },
        )
        .await
    }
}

impl Loaded {
    fn reusable(&self, key: &Key, now: Instant) -> bool {
        self.expires.is_some_and(|expiry| now < expiry)
            && (self.authorization.is_none() || self.authorization == key.authorization)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Public,
    Private,
}

pub(crate) struct Freshness {
    expires: Option<Instant>,
    authorization: Option<crate::oauth::GenerationId>,
    visibility: Option<Visibility>,
    reusable: bool,
}
impl Default for Freshness {
    fn default() -> Self {
        Self {
            expires: None,
            authorization: None,
            visibility: None,
            reusable: true,
        }
    }
}
impl Freshness {
    pub(crate) fn include(
        &mut self,
        result: &Value,
        received: Instant,
        generation: Option<crate::oauth::GenerationId>,
    ) {
        let visibility = match result["cacheScope"].as_str() {
            Some("public") => Visibility::Public,
            Some("private") => Visibility::Private,
            _ => {
                self.reusable = false;
                Visibility::Private
            }
        };
        if self.visibility.is_none() {
            self.authorization = generation;
            self.visibility = Some(visibility);
        } else if self.visibility != Some(visibility)
            || (visibility == Visibility::Private && generation != self.authorization)
        {
            self.reusable = false;
        }
        let ttl = result["ttlMs"]
            .as_f64()
            .filter(|ms| *ms > 0.0)
            .and_then(|ms| std::time::Duration::try_from_secs_f64(ms / 1000.0).ok());
        let expiry = ttl.and_then(|ttl| received.checked_add(ttl));
        if let Some(expiry) = expiry {
            self.expires = Some(self.expires.map_or(expiry, |old| old.min(expiry)));
        } else {
            self.reusable = false;
        }
    }

    pub(crate) fn invalidate(&mut self) {
        self.reusable = false;
    }

    pub(crate) fn list(self, tools: Vec<crate::RemoteTool>) -> Loaded {
        Loaded {
            tools: tools.into(),
            expires: if self.reusable { self.expires } else { None },
            authorization: if self.visibility == Some(Visibility::Private) {
                self.authorization
            } else {
                None
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summaries_are_deterministic_utf8_prefixes_with_a_byte_limit() {
        let text = "é".repeat(1000);
        let shortened = description(&text);
        assert!(shortened.len() <= 512);
        assert!(text.starts_with(shortened.strip_suffix('…').unwrap()));
        assert_eq!(shortened, description(&text));
    }
}
