//! The pages, the live transcript and the calls the browser makes.
//!
//! A chat page renders at once, without its transcript. Its tab then opens
//! an event stream: a session client of its own (the SDK's, as every app
//! has), whose updates the [`Page`] renders as patches. What the tab's
//! calls ask for goes to that client.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;

use client::{Attach, Cmd, Connection, Driver, Effect, Env, Event};
use http::header::HOST;
use http::{Method, Request, StatusCode};
use protocol::{ClientMsg, DaemonMsg, SessionId, SessionSummary, Target};
use rt::Rt;
use rt::channel::{Receiver, Sender};
use serde::Deserialize;

use crate::http::{Gate, Host, Reply, see_other, text, whole};
use crate::page::Page;
use crate::{Daemon, view};

/// Where the calls of the tabs whose transcripts are live go, by tab id,
/// with the number of the stream that opened it.
type Tabs = Mutex<HashMap<String, (u64, Arc<Sender<TabIn>>)>>;

/// What a live tab's client takes in.
enum TabIn {
    Client(Event),
    /// A line typed in the composer.
    Line(String),
    Cmd(Cmd),
}

pub struct App {
    daemon: Daemon,
    rt: Rt,
    pub gate: Gate,
    tabs: Arc<Tabs>,
    streams: AtomicU64,
}

// Nothing behind these locks is left half-updated by a panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a tab's calls carry: Datastar sends the page's signals.
#[derive(Deserialize)]
struct Signals {
    tab: String,
    #[serde(default)]
    draft: String,
}

impl App {
    pub fn new(daemon: Daemon, rt: Rt, gate: Gate) -> Self {
        Self {
            daemon,
            rt,
            gate,
            tabs: Arc::default(),
            streams: AtomicU64::new(0),
        }
    }

    /// Runs blocking daemon work off the async workers.
    fn blocking<T, F>(&self, work: F) -> impl Future<Output = T>
    where
        T: Send + 'static,
        F: FnOnce(&Daemon) -> T + Send + 'static,
    {
        let daemon = self.daemon.clone();
        rt::unblock(&*self.rt, move || work(&daemon))
    }

    /// Hands `what` to tab `tab`'s client; `false` if it has none.
    fn tell(&self, tab: &str, what: TabIn) -> bool {
        let Some((_, tx)) = lock(&self.tabs).get(tab).cloned() else {
            return false;
        };
        tx.send(what).is_ok()
    }

    /// Answers a request the gate let through.
    pub async fn handle(self: &Arc<Self>, request: Request<Vec<u8>>) -> Reply {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path();
        let query = parts.uri.query().unwrap_or_default();
        let host = parts
            .headers
            .get(HOST)
            .and_then(|h| self.gate.host(h.as_bytes()));
        let session = path.strip_prefix("/s/").map(|rest| {
            let (id, events) = rest
                .strip_suffix("/events")
                .map_or((rest, false), |id| (id, true));
            (parse_session(id), events)
        });
        let response = match (&parts.method, path, session) {
            (&Method::GET, "/", _) => {
                let listed = self.blocking(list).await.map_err(|e| e.to_string());
                page(view::sessions_page(&listed))
            }
            (&Method::POST, "/new", _) => match self.blocking(new_session).await {
                Ok(session) => see_other(&format!("/s/{session}"), None),
                Err(e) => text(
                    StatusCode::BAD_GATEWAY,
                    &format!("Could not reach the daemon: {e}"),
                ),
            },
            // On the main host, a chat would share its six connections
            // with every other tab's: it moves to a host of its own.
            (&Method::GET, _, Some((Some(_), false))) if host == Some(Host::Main) => {
                see_other(&self.gate.handoff(path), None)
            }
            (&Method::GET, _, Some((Some(session), false))) => page(view::chat_page(
                session,
                &uuid::Uuid::new_v4().simple().to_string(),
            )),
            (&Method::GET, _, Some((Some(session), true))) => {
                let Some(tab) = param(query, "tab").filter(|t| valid_tab(t)) else {
                    return Reply::Whole(text(StatusCode::NOT_FOUND, "no such tab"));
                };
                return Reply::Events(Box::new(Live::open(Arc::clone(self), tab, session)));
            }
            (&Method::POST, "/api/send", _) => match signals(&body) {
                Some(Signals { draft, .. }) if draft.trim().is_empty() => done(true),
                Some(Signals { tab, draft }) => {
                    if self.tell(&tab, TabIn::Line(draft)) {
                        // Empties the composer.
                        whole(
                            StatusCode::OK,
                            "application/json",
                            br#"{"draft":""}"#.to_vec(),
                        )
                    } else {
                        done(false)
                    }
                }
                None => done(false),
            },
            (&Method::POST, "/api/approve", _) => {
                let call = param(query, "call").and_then(|c| c.parse().ok());
                let approved = param(query, "ok") == Some("1");
                match (signals(&body), call) {
                    (Some(Signals { tab, .. }), Some(call)) => {
                        done(self.tell(&tab, TabIn::Cmd(Cmd::Approve { call, approved })))
                    }
                    _ => done(false),
                }
            }
            // Stops the running turn: its model or tool call, and the rest
            // of it.
            (&Method::POST, "/api/interrupt", _) => match signals(&body) {
                Some(Signals { tab, .. }) => done(self.tell(&tab, TabIn::Cmd(Cmd::Interrupt))),
                None => done(false),
            },
            _ => text(StatusCode::NOT_FOUND, "not found"),
        };
        Reply::Whole(response)
    }
}

fn page(html: String) -> http::Response<Vec<u8>> {
    whole(
        StatusCode::OK,
        "text/html; charset=utf-8",
        html.into_bytes(),
    )
}

/// A call's answer: nothing, or that the tab it names is not live.
fn done(told: bool) -> http::Response<Vec<u8>> {
    if told {
        whole(StatusCode::NO_CONTENT, "text/plain", Vec::new())
    } else {
        text(StatusCode::NOT_FOUND, "this tab is not live; reload it")
    }
}

fn signals(body: &[u8]) -> Option<Signals> {
    serde_json::from_slice(body).ok()
}

fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        kv.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

fn connect(daemon: &Daemon) -> io::Result<Connection> {
    client::connect(&daemon.endpoint, &daemon.staged, &|| {})
}

fn list(daemon: &Daemon) -> io::Result<Vec<SessionSummary>> {
    let mut sessions = connect(daemon)?.sessions()?;
    // Newest first: ids are UUIDv7.
    sessions.sort_by_key(|s| std::cmp::Reverse(s.id.0));
    Ok(sessions)
}

/// Starts a session in the workspace `a web` runs in, with its environment
/// (or a remote daemon's bridge's), as `a new` does.
fn new_session(daemon: &Daemon) -> io::Result<SessionId> {
    let new = Attach::new(Target::New, Env::Origin);
    let mut conn = client::connect_with(&daemon.endpoint, &daemon.staged, None, &|| {}, &|conn| {
        Ok(vec![conn.attach(&new)?])
    })?;
    let session = conn.reader.until(|msg| match msg {
        DaemonMsg::Attached { session, .. } => Some(session),
        _ => None,
    })?;
    conn.writer.send(&ClientMsg::Detach)?;
    Ok(session)
}

fn parse_session(id: &str) -> Option<SessionId> {
    uuid::Uuid::try_parse(id).ok().map(SessionId)
}

/// A tab id is 32 hex digits, as the chat page makes them.
fn valid_tab(tab: &str) -> bool {
    tab.len() == 32 && tab.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A tab's live transcript: its session client, the page rendered from it,
/// and the driver under it.
pub struct Live {
    app: Arc<App>,
    id: String,
    /// This stream's number: a reload's stream replaces it.
    stream: u64,
    inbox: Receiver<TabIn>,
    driver: Driver,
    page: Page,
}

impl Live {
    fn open(app: Arc<App>, id: &str, session: SessionId) -> Self {
        let (tx, inbox) = rt::channel::unbounded();
        let tx = Arc::new(tx);
        let events = Arc::clone(&tx);
        let daemon = app.daemon.clone();
        let driver = Driver::new(daemon.endpoint, daemon.staged, move |_, event| {
            let _ = events.send(TabIn::Client(event));
        });
        let stream = app.streams.fetch_add(1, Ordering::Relaxed);
        lock(&app.tabs).insert(id.to_owned(), (stream, tx));
        let (page, open) = Page::new(session);
        let mut live = Self {
            app,
            id: id.to_owned(),
            stream,
            inbox,
            driver,
            page,
        };
        live.apply(open);
        live
    }

    /// Has the driver carry out the client's effects.
    fn apply(&mut self, mut effects: Vec<Effect>) {
        while !effects.is_empty() {
            let shown = effects
                .into_iter()
                .filter_map(|effect| self.driver.apply(effect))
                .collect();
            effects = self.page.absorb(shown);
        }
    }

    /// The events to send next, waiting for the client; `None` once the
    /// transcript will not change again.
    pub async fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            let events = self.page.take();
            if !events.is_empty() {
                return Some(events);
            }
            if self.page.ended() {
                return None;
            }
            for input in self.batch().await? {
                let effects = match input {
                    TabIn::Client(event) => self.page.event(event),
                    TabIn::Line(text) => self.page.line(&text),
                    TabIn::Cmd(cmd) => self.page.command(cmd),
                };
                self.apply(effects);
            }
            self.page.settle();
        }
    }

    /// Everything that has arrived, waiting for at least one.
    async fn batch(&self) -> Option<Vec<TabIn>> {
        poll_fn(|cx| {
            let mut batch = Vec::new();
            loop {
                match self.inbox.poll_recv(cx) {
                    Poll::Ready(Some(input)) => batch.push(input),
                    Poll::Ready(None) => return Poll::Ready((!batch.is_empty()).then_some(batch)),
                    Poll::Pending if batch.is_empty() => return Poll::Pending,
                    Poll::Pending => return Poll::Ready(Some(batch)),
                }
            }
        })
        .await
    }
}

/// The tab closed (its stream was dropped) or reloaded: detach.
impl Drop for Live {
    fn drop(&mut self) {
        let mut tabs = lock(&self.app.tabs);
        if tabs.get(&self.id).is_some_and(|(s, _)| *s == self.stream) {
            tabs.remove(&self.id);
        }
        drop(tabs);
        let (driver, effects) = (self.driver.clone(), self.page.client.detach());
        self.app.rt.spawn_blocking(Box::new(move || {
            for effect in effects {
                driver.apply(effect);
            }
        }));
    }
}
