//! The pages, the live transcript and the calls the browser makes.
//!
//! A chat page renders right away; once the browser runtime is running, the
//! transcript shard asks again with `live` set and its response becomes a
//! long stream: it attaches as a daemon client of its own, folds what the
//! daemon sends through the shared reducer and emits the transcript again
//! after each batch. Input and approvals go out on that tab's connection.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;

use client::{Connection, Writer};
use protocol::view::{Change, Link, SessionView};
use protocol::{ClientMsg, DaemonMsg, SessionId, SessionSummary, Target};
use rt::Rt;
use rt::channel::Receiver;
use topcoat::Result;
use topcoat::asset::{AssetConfig, Manifest, ManifestEntry, RouterBuilderAssetExt};
use topcoat::context::{Cx, app_context};
use topcoat::router::error::{RouterErrorExt, SeeOther, see_other};
use topcoat::router::{Router, page, path_param, route};
use topcoat::runtime::{Event, RouterBuilderRuntimeExt, procedure, shard, signal};
use topcoat::view::{Child, View, component, emit, live, view};

use crate::view::transcript_column;
use crate::{Daemon, http, style};

/// Loaded by the chat page to learn when the runtime is running: the
/// runtime sets `src` again as it binds the image, and the second load
/// arrives after its `load` handler is attached.
const PIXEL: &str = "data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==";

pub fn router(daemon: Daemon, rt: Rt) -> Router {
    let script = ManifestEntry {
        id: topcoat::runtime::SCRIPT.id(),
        file: http::SCRIPT_PATH.trim_start_matches("/assets/").to_owned(),
        hash: String::new(),
        content_type: "text/javascript".to_owned(),
    };
    let manifest = Manifest {
        version: 1,
        assets: vec![script],
    };
    Router::builder()
        .assets(AssetConfig::hosted_at("/assets", manifest))
        .app_context(App {
            daemon,
            rt,
            tabs: Arc::default(),
        })
        .page(sessions_page)
        .page(chat_page)
        .route(create)
        .route(live_transcript)
        .route(send)
        .route(approve)
        .runtime()
        .build()
}

/// The writers of the tabs whose transcripts are live, by tab id.
type Tabs = Mutex<HashMap<String, Arc<Mutex<Writer>>>>;

struct App {
    daemon: Daemon,
    rt: Rt,
    tabs: Arc<Tabs>,
}

// Nothing behind these locks is left half-updated by a panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl App {
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

/// A tab id is 32 hex digits, as [`chat_page`] makes them.
fn valid_tab(tab: &str) -> bool {
    tab.len() == 32 && tab.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One browser tab's live transcript: a daemon connection of its own, whose
/// writer the tab's calls use while it lasts.
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

/// The tab closed (its response was dropped) or reconnected: detach.
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

#[component]
async fn document(title: String, #[default] child: Child<'_>) -> Result<impl View> {
    Ok(view! {
        <!DOCTYPE html>
        <html lang="en" style=(style::ROOT)>
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <title>(title)</title>
                topcoat::runtime::script()
            </head>
            <body style=(style::BODY)>(child)</body>
        </html>
    })
}

#[page("/")]
async fn sessions_page(cx: &Cx) -> Result<impl View> {
    let listed = app_context::<App>(cx).blocking(list).await;
    Ok(view! {
        document(title: "a · sessions".to_owned(),
            <header style=(style::BAR)>
                <span style=(style::BRAND)>"a"</span>
                <span style=(style::DIM)>"sessions"</span>
                <form method="post" action="/new" style="margin: 0 0 0 auto;">
                    <button type="submit" style=(style::BUTTON)>"New session"</button>
                </form>
            </header>
            <main style=(style::MAIN)>
                <div style=(style::COLUMN)>
                    match listed {
                        Ok(sessions) => {
                            if sessions.is_empty() {
                                <p style=(style::NOTE)>"No sessions yet."</p>
                            }
                            #[key(summary.id.0.as_u128())]
                            for summary in sessions {
                                <a style=(style::CARD) href=(format!("/s/{}", summary.id))>
                                    <span style=(style::MONO)>(summary.id.short().to_string())</span>
                                    <span style="flex: 1; min-width: 12rem; overflow-wrap: anywhere;">(summary.workspace.display().to_string())</span>
                                    <span style=(style::DIM)>
                                        (summary.entries) " entries"
                                        if summary.clients > 0 {
                                            " · " (summary.clients) " attached"
                                        }
                                    </span>
                                </a>
                            }
                        }
                        Err(e) => {
                            <p style=(style::BAD)>"Could not reach the daemon: " (e.to_string())</p>
                        }
                    }
                </div>
            </main>
        )
    })
}

#[route(POST "/new")]
async fn create(cx: &Cx) -> Result<SeeOther> {
    let session = app_context::<App>(cx).blocking(new_session).await?;
    Ok(see_other(format!("/s/{session}")))
}

path_param!(session);

#[page("/s/{session}")]
async fn chat_page(cx: &Cx) -> Result<impl View> {
    let session = parse_session(path_param::<Session>(cx)).ok_or_not_found()?;
    let tab = uuid::Uuid::new_v4().simple().to_string();
    let live = signal(cx, || false);
    let pixel = signal(cx, || PIXEL.to_owned());
    let draft = signal(cx, String::new);
    Ok(view! {
        document(title: format!("a · {}", session.short()),
            <header style=(style::BAR)>
                <a style=(style::BRAND) href="/">"← a"</a>
                <span style=(style::MONO)>(session.short().to_string())</span>
            </header>
            <img hidden=(true) alt="" :src=$(pixel.get()) @load=$(|_e: Event| live.set(true))>
            <main style=(style::SCROLLER)>
                live_transcript(tab: tab.clone(), session: session.to_string(), live: $(live.get()))
            </main>
            <div style=(style::COMPOSER_BAR)>
                <form style=(style::COMPOSER) @submit=$(async |e: Event| {
                    e.prevent_default();
                    let text = draft.get();
                    if !text.trim().is_empty() {
                        if send(tab, text).await {
                            draft.set("".to_owned());
                        }
                    }
                })>
                    <input style=(style::INPUT) name="text" placeholder="Message" autocomplete="off" autofocus=(true)
                        :value=$(draft.get()) @input=$(|e: Event| draft.set(e.target.value))>
                    <button type="submit" style=(style::BUTTON)>"Send"</button>
                </form>
            </div>
        )
    })
}

/// The live transcript of `session` in browser tab `tab`. Rendered inline
/// with the page while `live` is false; the request with `live` set streams
/// for as long as the tab stays open.
#[shard("/api/transcript")]
async fn live_transcript(cx: &Cx, tab: String, session: String, live: bool) -> Result<impl View> {
    let session = parse_session(&session).ok_or_not_found()?;
    let tab = Some(tab).filter(|t| valid_tab(t)).ok_or_not_found()?;
    Ok(live! {
        if !live {
            return emit! { <p style=(style::NOTE)>"Connecting…"</p> };
        }
        let app = app_context::<App>(cx);
        let mut view = SessionView::default();
        // The daemon's latest complaint, until the transcript moves on.
        let mut error = None;
        let mut conn = Tab::open(app, &tab, session, 0).await;
        loop {
            let connection = match &conn {
                Ok(connection) => connection,
                Err(e) => {
                    break emit! { <p style=(style::BAD)>"Could not attach: " (e.to_string())</p> };
                }
            };
            match connection.next().await {
                Some(batch) => {
                    for msg in &batch {
                        match view.fold(msg).map(|update| update.change) {
                            Some(Change::Error { message }) => error = Some(message),
                            Some(Change::Added { .. }) => error = None,
                            _ => {}
                        }
                    }
                    emit! { transcript_column(view: view.clone(), tab: tab.clone(), error: error.clone(), ended: false) }?;
                }
                // Upgraded: the new daemon replays what this tab missed.
                None if view.link == Link::Draining => {
                    conn = Tab::open(app, &tab, session, view.seq).await;
                }
                None => break emit! { transcript_column(view: view.clone(), tab: tab.clone(), error: error.clone(), ended: true) },
            }
        }
    })
}

#[procedure("/api/send")]
async fn send(cx: &Cx, tab: String, text: String) -> Result<bool> {
    if text.trim().is_empty() {
        return Ok(false);
    }
    Ok(app_context::<App>(cx)
        .tell(&tab, ClientMsg::input(text))
        .await)
}

#[procedure("/api/approve")]
pub async fn approve(cx: &Cx, tab: String, call: u64, approved: bool) -> Result<bool> {
    Ok(app_context::<App>(cx)
        .tell(&tab, ClientMsg::approve(call, approved))
        .await)
}
