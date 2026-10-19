//! The pages, the live transcript and the calls the browser makes.
//!
//! A chat page renders at once, without its transcript. Its tab then opens
//! an event stream: it attaches as a daemon client of its own and sends
//! what the [`Page`] folded from the daemon's messages makes of them. Input
//! and approvals go out on that tab's connection.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;

use client::{Connection, Writer};
use http::header::HOST;
use http::{Method, Request, StatusCode};
use protocol::view::Link;
use protocol::{ClientMsg, DaemonMsg, SessionId, SessionSummary, Target};
use rt::Rt;
use rt::channel::Receiver;
use serde::Deserialize;

use crate::http::{Gate, Host, Reply, see_other, text, whole};
use crate::page::Page;
use crate::{Daemon, view};

/// The writers of the tabs whose transcripts are live, by tab id.
type Tabs = Mutex<HashMap<String, Arc<Mutex<Writer>>>>;

pub struct App {
    daemon: Daemon,
    rt: Rt,
    pub gate: Gate,
    tabs: Arc<Tabs>,
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

    /// Sends `msg` on tab `tab`'s connection; `false` if it has none.
    async fn tell(&self, tab: &str, msg: ClientMsg) -> bool {
        let Some(writer) = lock(&self.tabs).get(tab).cloned() else {
            return false;
        };
        rt::unblock(&*self.rt, move || lock(&writer).send(&msg).is_ok()).await
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
                return Reply::Events(Box::new(Live::open(Arc::clone(self), tab, session).await));
            }
            (&Method::POST, "/api/send", _) => match signals(&body) {
                Some(Signals { draft, .. }) if draft.trim().is_empty() => done(true),
                Some(Signals { tab, draft }) => {
                    if self.tell(&tab, ClientMsg::input(draft)).await {
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
                        done(self.tell(&tab, ClientMsg::approve(call, approved)).await)
                    }
                    _ => done(false),
                }
            }
            // Stops the running turn: its model or tool call, and the rest
            // of it.
            (&Method::POST, "/api/interrupt", _) => match signals(&body) {
                Some(Signals { tab, .. }) => done(self.tell(&tab, ClientMsg::INTERRUPT).await),
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
    let mut conn = client::connect_with(&daemon.endpoint, &daemon.staged, &|| {}, &|conn| {
        Ok(vec![conn.attach(Target::New, 0, true)?])
    })?;
    let session = conn.reader.until(|msg| match msg {
        DaemonMsg::Attached { session, .. } => Some(session),
        _ => None,
    })?;
    conn.writer.send(&ClientMsg::Detach)?;
    Ok(session)
}

/// Attaches to `session` without an environment, so it keeps the one it
/// has, replaying the entries after `since`.
fn attach(daemon: &Daemon, session: SessionId, since: u64) -> io::Result<Connection> {
    client::connect_with(&daemon.endpoint, &daemon.staged, &|| {}, &|conn| {
        Ok(vec![conn.attach(Target::Id(session), since, false)?])
    })
}

fn parse_session(id: &str) -> Option<SessionId> {
    uuid::Uuid::try_parse(id).ok().map(SessionId)
}

/// A tab id is 32 hex digits, as the chat page makes them.
fn valid_tab(tab: &str) -> bool {
    tab.len() == 32 && tab.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One browser tab's daemon connection, whose writer the tab's calls use
/// while it lasts.
struct Tab {
    id: String,
    messages: Receiver<DaemonMsg>,
    writer: Arc<Mutex<Writer>>,
    tabs: Arc<Tabs>,
    rt: Rt,
}

impl Tab {
    async fn open(app: &App, id: &str, session: SessionId, since: u64) -> io::Result<Self> {
        let Connection {
            mut reader, writer, ..
        } = app
            .blocking(move |daemon| attach(daemon, session, since))
            .await?;
        let (tx, messages) = rt::channel::bounded(256);
        // Ends when the daemon closes the connection, as it does on detach.
        app.rt.spawn_blocking(Box::new(move || {
            while let Ok(Some(msg)) = reader.recv() {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        }));
        let writer = Arc::new(Mutex::new(writer));
        lock(&app.tabs).insert(id.to_owned(), Arc::clone(&writer));
        Ok(Self {
            id: id.to_owned(),
            messages,
            writer,
            tabs: Arc::clone(&app.tabs),
            rt: Arc::clone(&app.rt),
        })
    }

    /// Every message that has arrived, waiting for at least one; `None`
    /// once the daemon closed the connection.
    async fn next(&self) -> Option<Vec<DaemonMsg>> {
        poll_fn(|cx| {
            let mut batch = Vec::new();
            loop {
                match self.messages.poll_recv(cx) {
                    Poll::Ready(Some(msg)) => batch.push(msg),
                    Poll::Ready(None) => return Poll::Ready((!batch.is_empty()).then_some(batch)),
                    Poll::Pending if batch.is_empty() => return Poll::Pending,
                    Poll::Pending => return Poll::Ready(Some(batch)),
                }
            }
        })
        .await
    }
}

/// The tab closed (its stream was dropped) or reconnected: detach.
impl Drop for Tab {
    fn drop(&mut self) {
        let mut tabs = lock(&self.tabs);
        if tabs
            .get(&self.id)
            .is_some_and(|w| Arc::ptr_eq(w, &self.writer))
        {
            tabs.remove(&self.id);
        }
        let writer = Arc::clone(&self.writer);
        self.rt.spawn_blocking(Box::new(move || {
            let _ = lock(&writer).send(&ClientMsg::Detach);
        }));
    }
}

/// A tab's live transcript: its daemon connection and the page folded
/// from it.
pub struct Live {
    app: Arc<App>,
    id: String,
    session: SessionId,
    /// `None` once the daemon is gone for good.
    tab: Option<Tab>,
    page: Page,
}

impl Live {
    async fn open(app: Arc<App>, id: &str, session: SessionId) -> Self {
        let tab = Tab::open(&app, id, session, 0).await;
        let mut page = Page::default();
        let tab = tab
            .map_err(|e| page.end(Some(format!("Could not attach: {e}"))))
            .ok();
        Self {
            app,
            id: id.to_owned(),
            session,
            tab,
            page,
        }
    }

    /// The events to send next, waiting for the daemon; `None` once the
    /// transcript will not change again.
    pub async fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            let events = self.page.take();
            if !events.is_empty() {
                return Some(events);
            }
            let batch = self.tab.as_ref()?.next().await;
            match batch {
                Some(batch) => self.page.fold(&batch),
                None => self.lost().await,
            }
        }
    }

    /// The daemon closed the connection: after an upgrade, the new daemon
    /// replays what this tab missed.
    async fn lost(&mut self) {
        self.tab = None;
        if self.page.view.link != Link::Draining {
            return self.page.end(None);
        }
        let since = self.page.view.seq;
        match Tab::open(&self.app, &self.id, self.session, since).await {
            Ok(tab) => self.tab = Some(tab),
            Err(e) => self.page.end(Some(format!("Could not attach: {e}"))),
        }
    }
}
