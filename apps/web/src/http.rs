//! A small HTTP/1.1 server for one local user: one request per connection,
//! answered whole, or as an event stream that lasts until the tab goes
//! away. Socket IO blocks, so it runs on the runtime's blocking threads;
//! the app runs on its async workers.

use std::future::poll_fn;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::pin::{Pin, pin};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::{Duration, Instant};

use http::header::{
    CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, LOCATION, ORIGIN,
    SET_COOKIE,
};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use rt::Rt;
use rt::oneshot::{self, Receiver};

use crate::app::{App, Live};

/// Where the browser script is served. The version in the path lets
/// browsers keep it.
pub const SCRIPT_PATH: &str = "/assets/datastar-1.0.4.js";

/// Datastar's browser script, as published (`bundles/datastar.js` of
/// v1.0.4, MIT; SHA-256 727844ad…cf91), zlib-deflated by the build script
/// (34 KB to 13).
pub const SCRIPT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/datastar.js.zz"));

const COOKIE_NAME: &str = "a-web";
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 2 * 1024 * 1024;
/// How long a tab host's sign-in token stays good: long enough for the
/// browser to follow the redirect that carries it.
const HANDOFF: Duration = Duration::from_secs(60);

/// What a request is answered with.
pub enum Reply {
    Whole(Response<Vec<u8>>),
    /// A `text/event-stream` of what the live tab is sent.
    Events(Box<Live>),
}

/// Who may use the UI: requests addressed to this port on a loopback host,
/// from pages of those hosts, by the browser that redeemed the one-time
/// link.
///
/// Each chat tab gets a host of its own (`<label>.localhost`), since
/// browsers open at most six HTTP/1.1 connections to a host and each tab
/// holds one open for its event stream. A cookie is per host, so a tab
/// host is signed in by a short-lived token of its own.
pub struct Gate {
    port: u16,
    /// Taken when the link is first opened.
    token: Mutex<Option<String>>,
    /// Tab hosts' tokens, each good once until its deadline.
    handoffs: Mutex<Vec<(String, Instant)>>,
    cookie: String,
}

/// How a host addresses this server.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Host {
    /// `127.0.0.1` or `localhost`.
    Main,
    /// A `<label>.localhost` of a tab.
    Tab,
}

impl Gate {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            token: Mutex::new(Some(secret())),
            handoffs: Mutex::default(),
            cookie: secret(),
        }
    }

    /// The one-time link to print and open.
    pub fn link(&self) -> String {
        let token = self.token.lock().unwrap_or_else(PoisonError::into_inner);
        format!(
            "http://127.0.0.1:{}/?token={}",
            self.port,
            token.as_deref().unwrap_or_default()
        )
    }

    /// Where to open `path` on a new tab host, signed in by a token good
    /// once, for a while.
    pub fn handoff(&self, path: &str) -> String {
        let token = secret();
        let mut handoffs = self.handoffs.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        handoffs.retain(|(_, deadline)| *deadline > now);
        handoffs.push((token.clone(), now + HANDOFF));
        // Any label will do: random, so no two tabs share a host.
        let label = &secret()[..12];
        format!(
            "http://t{label}.localhost:{}{path}?token={token}",
            self.port
        )
    }

    /// How `host` (a Host header's value) addresses this server, if it does.
    pub fn host(&self, host: &[u8]) -> Option<Host> {
        let host = std::str::from_utf8(host).ok()?;
        let (name, port) = host.rsplit_once(':')?;
        if port.parse() != Ok(self.port) {
            return None;
        }
        match name {
            "127.0.0.1" | "localhost" => Some(Host::Main),
            _ => {
                let label = name.strip_suffix(".localhost")?;
                let valid = !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-');
                valid.then_some(Host::Tab)
            }
        }
    }

    /// Lets `request` through, with the cookie to set if it just signed
    /// its host in, or answers it itself: one from elsewhere, or the
    /// link's redemption.
    fn answer(
        &self,
        request: &Request<Vec<u8>>,
    ) -> Result<Option<HeaderValue>, Box<Response<Vec<u8>>>> {
        let headers = request.headers();
        // A foreign Host is DNS rebinding; a foreign Origin, another site.
        if headers
            .get(HOST)
            .and_then(|h| self.host(h.as_bytes()))
            .is_none()
        {
            return Err(Box::new(text(
                StatusCode::MISDIRECTED_REQUEST,
                "unknown host",
            )));
        }
        let origin = headers.get(ORIGIN).map(HeaderValue::as_bytes);
        if origin.is_some_and(|o| {
            o.strip_prefix(b"http://")
                .and_then(|host| self.host(host))
                .is_none()
        }) {
            return Err(Box::new(text(
                StatusCode::FORBIDDEN,
                "cross-origin request",
            )));
        }
        let signed_in = cookies(headers).any(|(name, value)| {
            name == COOKIE_NAME && same(value.as_bytes(), self.cookie.as_bytes())
        });
        let offered = request
            .uri()
            .query()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")));
        let path = request.uri().path();
        match offered {
            Some(_) if signed_in => Err(Box::new(see_other(path, None))),
            Some(offered) => {
                let cookie = format!(
                    "{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Strict",
                    self.cookie
                );
                let cookie = HeaderValue::try_from(cookie).expect("hex is a header value");
                if self.redeem_link(offered) {
                    return Err(Box::new(see_other(path, Some(cookie))));
                }
                // The redirect here came from another site (127.0.0.1), so
                // a strict cookie would not come with a redirect back: the
                // page is the answer, and sets the cookie.
                if self.redeem_handoff(offered) {
                    return Ok(Some(cookie));
                }
                Err(Box::new(text(
                    StatusCode::FORBIDDEN,
                    "this link was used already; run `a web` again for a new one",
                )))
            }
            None if signed_in => Ok(None),
            None => Err(Box::new(text(
                StatusCode::FORBIDDEN,
                "open the link `a web` printed to sign in",
            ))),
        }
    }

    fn redeem_link(&self, offered: &str) -> bool {
        let mut token = self.token.lock().unwrap_or_else(PoisonError::into_inner);
        let ok = token
            .as_deref()
            .is_some_and(|t| same(t.as_bytes(), offered.as_bytes()));
        if ok {
            *token = None;
        }
        ok
    }

    fn redeem_handoff(&self, offered: &str) -> bool {
        let mut handoffs = self.handoffs.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        handoffs.retain(|(_, deadline)| *deadline > now);
        let found = handoffs
            .iter()
            .position(|(t, _)| same(t.as_bytes(), offered.as_bytes()));
        found.map(|i| handoffs.swap_remove(i)).is_some()
    }
}

/// 122 random bits, as hex.
fn secret() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Compares secrets in time independent of where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn cookies(headers: &HeaderMap) -> impl Iterator<Item = (&str, &str)> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
}

/// A response of `body` as `content_type`.
pub fn whole(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Response<Vec<u8>> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

pub fn text(status: StatusCode, message: &str) -> Response<Vec<u8>> {
    whole(
        status,
        "text/plain; charset=utf-8",
        format!("{message}\n").into_bytes(),
    )
}

pub fn see_other(location: &str, cookie: Option<HeaderValue>) -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = StatusCode::SEE_OTHER;
    let headers = response.headers_mut();
    if let Ok(location) = HeaderValue::try_from(location) {
        headers.insert(LOCATION, location);
    }
    if let Some(cookie) = cookie {
        headers.insert(SET_COOKIE, cookie);
    }
    response
}

/// Serves one connection: a single request, then closes.
pub async fn serve(rt: Rt, app: Arc<App>, stream: TcpStream) {
    let stream = Arc::new(stream);
    let _ = respond(&rt, &app, &stream).await;
    // Also wakes the probe of `closed`, which holds a duplicate.
    let _ = stream.shutdown(Shutdown::Both);
}

async fn respond(rt: &Rt, app: &Arc<App>, stream: &Arc<TcpStream>) -> io::Result<()> {
    let from = Arc::clone(stream);
    let request = match rt::unblock(&**rt, move || read_request(&from)).await {
        Ok(request) => request,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            return send_whole(rt, stream, text(StatusCode::BAD_REQUEST, &e.to_string())).await;
        }
        Err(e) => return Err(e),
    };
    let cookie = match app.gate.answer(&request) {
        Ok(cookie) => cookie,
        Err(response) => return send_whole(rt, stream, *response).await,
    };
    if request.uri().path() == SCRIPT_PATH {
        // Every browser takes deflate; only browsers load the script.
        let mut response = whole(StatusCode::OK, "text/javascript", SCRIPT.to_vec());
        let headers = response.headers_mut();
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("deflate"));
        headers.insert(
            CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=31536000, immutable"),
        );
        return send_whole(rt, stream, response).await;
    }
    match app.handle(request).await {
        Reply::Whole(mut response) => {
            if let Some(cookie) = cookie {
                response.headers_mut().insert(SET_COOKIE, cookie);
            }
            send_whole(rt, stream, response).await
        }
        Reply::Events(live) => send_events(rt, stream, live).await,
    }
}

fn read_request(mut stream: &TcpStream) -> io::Result<Request<Vec<u8>>> {
    let invalid =
        |e: &dyn std::fmt::Display| io::Error::new(io::ErrorKind::InvalidData, e.to_string());
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut headers);
        let httparse::Status::Complete(head) = parsed.parse(&buf).map_err(|e| invalid(&e))? else {
            if buf.len() > MAX_HEAD {
                return Err(invalid(&"request head too large"));
            }
            continue;
        };
        let mut request = Request::builder()
            .method(parsed.method.unwrap_or("GET"))
            .uri(parsed.path.unwrap_or("/"));
        let mut length = 0;
        for header in parsed.headers.iter() {
            if header.name.eq_ignore_ascii_case(CONTENT_LENGTH.as_str()) {
                length = std::str::from_utf8(header.value)
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .ok_or_else(|| invalid(&"bad content-length"))?;
            }
            request = request.header(header.name, header.value);
        }
        if length > MAX_BODY {
            return Err(invalid(&"request body too large"));
        }
        let mut body = buf[head..].to_vec();
        while body.len() < length {
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(length);
        return request.body(body).map_err(|e| invalid(&e));
    }
}

/// Ends when the browser closes the connection, which it does when the tab
/// goes away: a stream waiting on the daemon would otherwise notice only
/// on its next write.
fn closed(rt: &Rt, stream: &TcpStream) -> io::Result<Receiver<()>> {
    let mut probe = stream.try_clone()?;
    let (tx, rx) = oneshot::channel();
    rt.spawn_blocking(Box::new(move || {
        // One request per connection: anything read now is the end.
        let _ = probe.read(&mut [0; 1]);
        drop(tx);
    }));
    Ok(rx)
}

fn head(status: StatusCode, headers: &HeaderMap) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status}\r\n").into_bytes();
    for (name, value) in headers {
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    if !headers.contains_key(CACHE_CONTROL) {
        head.extend_from_slice(b"cache-control: no-store\r\n");
    }
    head.extend_from_slice(
        b"x-content-type-options: nosniff\r\n\
          referrer-policy: same-origin\r\n\
          content-security-policy: frame-ancestors 'none'\r\n\
          connection: close\r\n",
    );
    head
}

async fn write(rt: &Rt, stream: &Arc<TcpStream>, bytes: Vec<u8>) -> io::Result<()> {
    let stream = Arc::clone(stream);
    rt::unblock(&**rt, move || (&*stream).write_all(&bytes)).await
}

async fn send_whole(
    rt: &Rt,
    stream: &Arc<TcpStream>,
    response: Response<Vec<u8>>,
) -> io::Result<()> {
    let (parts, body) = response.into_parts();
    let mut bytes = head(parts.status, &parts.headers);
    bytes.extend_from_slice(format!("content-length: {}\r\n\r\n", body.len()).as_bytes());
    bytes.extend_from_slice(&body);
    write(rt, stream, bytes).await
}

/// Writes each batch of events as soon as `live` has it, until it ends or
/// the browser goes away. The body runs to the end of the connection.
async fn send_events(rt: &Rt, stream: &Arc<TcpStream>, mut live: Box<Live>) -> io::Result<()> {
    let mut closed = closed(rt, stream)?;
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    let mut bytes = head(StatusCode::OK, &headers);
    bytes.extend_from_slice(b"\r\n");
    write(rt, stream, bytes).await?;
    loop {
        let next = {
            let mut events = pin!(live.next());
            poll_fn(|cx| {
                if Pin::new(&mut closed).poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
                events.as_mut().poll(cx)
            })
            .await
        };
        // The stream ended, or the browser left.
        let Some(events) = next else { return Ok(()) };
        write(rt, stream, events).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::{Gate, Host, SCRIPT, SCRIPT_PATH};

    /// The embedded script is the one its path names.
    #[test]
    fn the_embedded_script_is_the_published_one() {
        let script = miniz_oxide::inflate::decompress_to_vec_zlib(SCRIPT).unwrap();
        let version = SCRIPT_PATH
            .strip_prefix("/assets/datastar-")
            .and_then(|v| v.strip_suffix(".js"))
            .unwrap();
        assert!(
            script.starts_with(format!("// Datastar v{version}\n").as_bytes()),
            "{}",
            String::from_utf8_lossy(&script[..40])
        );
    }

    #[test]
    fn a_host_is_ours_on_our_port_only() {
        let gate = Gate::new(4000);
        let host = |h: &str| gate.host(h.as_bytes());
        assert!(host("127.0.0.1:4000") == Some(Host::Main));
        assert!(host("localhost:4000") == Some(Host::Main));
        assert!(host("t0a1-b.localhost:4000") == Some(Host::Tab));
        for foreign in [
            "127.0.0.1:4001",
            "127.0.0.1",
            "t1.localhost:40000",
            ".localhost:4000",
            "a.b.localhost:4000",
            "evil.test:4000",
            "localhost.evil.test:4000",
            "[::1]:4000",
        ] {
            assert!(host(foreign).is_none(), "{foreign}");
        }
        let handoff = gate.handoff("/s/x");
        let rest = handoff.strip_prefix("http://").unwrap();
        let (moved, path) = rest.split_once('/').unwrap();
        assert!(host(moved) == Some(Host::Tab), "{handoff}");
        assert!(path.starts_with("s/x?token="), "{handoff}");
    }
}
