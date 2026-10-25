//! The pages, the live transcript and the calls the browser makes.
//!
//! A chat page renders at once, without its transcript. Its tab then opens
//! an event stream: a session client of its own (the SDK's, as every app
//! has), whose updates the [`Page`] renders as patches. What the tab's
//! calls ask for goes to that client.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;

use client::{Cmd, Daemons, Driver, Event};
use client::{Inbox, Post};
use http::header::HOST;
use http::{Method, Request, StatusCode};
use protocol::{SessionId, SessionSummary};
use rt::Rt;
use serde::Deserialize;

use crate::http::{Gate, Host, Reply, see_other, text, whole};
use crate::page::Page;
use crate::view;

/// Where the calls of the tabs whose transcripts are live go, by tab id,
/// with the number of the stream that opened it.
type Tabs = Mutex<HashMap<String, (u64, Arc<Post<TabIn>>)>>;

/// What a live tab's client takes in.
enum TabIn {
    Client(Event),
    /// A line typed in the composer.
    Line(String),
    Cmd(Cmd),
}

pub struct App {
    /// Its chats are the first member's.
    daemons: Daemons,
    rt: Rt,
    pub gate: Gate,
    tabs: Arc<Tabs>,
    streams: AtomicU64,
}

// Nothing behind these locks is left half-updated by a panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every member's sessions, side by side; an error only when no member
/// could list.
fn listed(daemons: &Daemons) -> Result<Vec<SessionSummary>, String> {
    let (found, failed): (Vec<_>, Vec<_>) = daemons
        .sessions()
        .into_iter()
        .map(|(_, listing)| listing)
        .partition(Result::is_ok);
    match failed.into_iter().next() {
        Some(Err(e)) if found.is_empty() => Err(e.to_string()),
        _ => Ok(found
            .into_iter()
            .flatten()
            .flatten()
            .map(|l| l.summary)
            .collect()),
    }
}

/// What a tab's calls carry: Datastar sends the page's signals.
#[derive(Deserialize)]
struct Signals {
    tab: String,
    #[serde(default)]
    draft: String,
}

impl App {
    pub fn new(daemons: Daemons, rt: Rt, gate: Gate) -> Self {
        Self {
            daemons,
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
        F: FnOnce(&Daemons) -> T + Send + 'static,
    {
        let daemons = self.daemons.clone();
        rt::unblock(&*self.rt, move || work(&daemons))
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
                let listed = self.blocking(listed).await;
                page(view::sessions_page(&listed))
            }
            (&Method::POST, "/new", _) => match self
                .blocking(|daemons| daemons.first().daemon.new_session())
                .await
            {
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
                let call = param(query, "call")
                    .and_then(|c| c.parse().ok().and_then(protocol::CallId::new));
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
    inbox: Inbox<TabIn>,
    driver: client::ClientHandle,
    page: Page,
}

impl Live {
    fn open(app: Arc<App>, id: &str, session: SessionId) -> Self {
        let (tx, inbox) = client::inbox(client::QueueBudget::default(), |input| {
            size_of::<TabIn>()
                + match input {
                    TabIn::Client(event) => event.retained_bytes(),
                    TabIn::Line(text) => text.len(),
                    TabIn::Cmd(_) => 4096,
                }
        });
        let tx = Arc::new(tx);
        let events = Arc::clone(&tx);
        let driver = Driver::new(move |driver, event| events.event(driver, event, TabIn::Client))
            .start(app.rt.clone(), app.daemons.first().daemon.clone());
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
        live.driver.pump(&mut live.page, open);
        live
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
            for input in Self::batch(&self.inbox).await? {
                let effects = match input {
                    TabIn::Client(event) => self.page.client.step(event),
                    TabIn::Line(text) => self.page.line(&text),
                    TabIn::Cmd(cmd) => self.page.command(cmd),
                };
                self.driver.pump(&mut self.page, effects);
            }
            self.page.settle();
            rt::yield_now().await;
        }
    }

    /// Everything that has arrived, waiting for at least one.
    async fn batch(inbox: &Inbox<TabIn>) -> Option<Vec<TabIn>> {
        poll_fn(|cx| {
            let mut batch = Vec::new();
            while batch.len() < client::PUMP_BATCH {
                match inbox.poll_recv(cx) {
                    Poll::Ready(Some(input)) => batch.push(input),
                    Poll::Ready(None) => return Poll::Ready((!batch.is_empty()).then_some(batch)),
                    Poll::Pending if batch.is_empty() => return Poll::Pending,
                    Poll::Pending => return Poll::Ready(Some(batch)),
                }
            }
            Poll::Ready(Some(batch))
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
        self.driver.pump(&mut (), self.page.client.detach());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sustained_input_cannot_starve_rendering() {
        let (post, inbox) = client::inbox(client::QueueBudget::default(), |_| 64);
        let input = |n: usize| {
            TabIn::Client(Event {
                conn: client::ConnId(1),
                kind: client::EventKind::Msg(protocol::DaemonMsg::Error {
                    message: n.to_string(),
                }),
            })
        };
        for n in 0..client::PUMP_BATCH * 2 {
            post.send(input(n)).ok().unwrap();
        }
        let (mut page, _) = Page::new(SessionId::new());
        for kind in [
            client::EventKind::Connected {
                welcome: client::Welcome {
                    proto: protocol::PROTO,
                    caps: protocol::Caps::ALL,
                },
                remote: false,
                commands: Vec::new(),
                machine: protocol::MachineId([0; 16]),
            },
            client::EventKind::Msg(protocol::DaemonMsg::Synced { seq: 0 }),
        ] {
            let effects = page.client.step(Event {
                conn: client::ConnId(1),
                kind,
            });
            client::pump(|_| Ok(()), &mut page, effects);
        }
        page.take();
        let mut next = client::PUMP_BATCH * 2;
        for _ in 0..3 {
            let batch = rt::block_on(Live::batch(&inbox)).unwrap();
            assert_eq!(batch.len(), client::PUMP_BATCH);
            for queued in batch {
                let TabIn::Client(event) = queued else {
                    panic!("daemon event")
                };
                let effects = page.client.step(event);
                client::pump(|_| Ok(()), &mut page, effects);
                post.send(input(next)).ok().unwrap();
                next += 1;
            }
            page.settle();
            assert_ne!(page.take(), [] as [u8; 0]);
            assert!(inbox.try_recv().is_some());
            post.send(input(next)).ok().unwrap();
            next += 1;
        }
    }
}
