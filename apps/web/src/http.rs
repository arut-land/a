//! A small HTTP/1.1 server for one local user: one request per connection,
//! and responses streamed chunk by chunk as the router produces them, since
//! a live transcript is one long response. Socket IO blocks, so it runs on
//! the runtime's blocking threads; the router runs on its async workers.

use std::future::{Future, poll_fn};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::pin::pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;

use http::header::{CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
use http_body_util::BodyExt;
use rt::Rt;
use rt::channel::Receiver;
use topcoat::router::{Body, Router};

/// Where the browser runtime is served; see [`crate::app::router`].
pub const SCRIPT_PATH: &str = "/assets/topcoat.js";

/// topcoat's browser runtime, as published in `topcoat-runtime` 0.10.0
/// (`browser/dist/index.js`, MIT). Bump it with the `topcoat` dependency;
/// a test checks they match.
pub const SCRIPT: &[u8] = include_bytes!("../assets/topcoat-runtime-0.10.0.js");

const COOKIE_NAME: &str = "a-web";
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 2 * 1024 * 1024;

/// Who may use the UI: requests addressed to this port on the loopback
/// host, from pages of this origin, by the browser that redeemed the
/// one-time link.
pub struct Gate {
    port: u16,
    /// Taken when the link is first opened.
    token: Mutex<Option<String>>,
    cookie: String,
}

impl Gate {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            token: Mutex::new(Some(secret())),
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

    /// The response for a request the gate answers itself: one from
    /// elsewhere, or the link's redemption. `None` lets it through.
    fn answer(&self, request: &Request<Vec<u8>>) -> Option<Response<Vec<u8>>> {
        let headers = request.headers();
        let ours = |host: &[u8]| {
            host == format!("127.0.0.1:{}", self.port).as_bytes()
                || host == format!("localhost:{}", self.port).as_bytes()
        };
        // A foreign Host is DNS rebinding; a foreign Origin, another site.
        if !headers.get(HOST).is_some_and(|h| ours(h.as_bytes())) {
            return Some(text(StatusCode::MISDIRECTED_REQUEST, "unknown host"));
        }
        let origin = headers.get(ORIGIN).map(HeaderValue::as_bytes);
        if origin.is_some_and(|o| !o.strip_prefix(b"http://").is_some_and(ours)) {
            return Some(text(StatusCode::FORBIDDEN, "cross-origin request"));
        }
        let signed_in = cookies(headers).any(|(name, value)| {
            name == COOKIE_NAME && same(value.as_bytes(), self.cookie.as_bytes())
        });
        let offered = request
            .uri()
            .query()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")));
        match offered {
            Some(_) if signed_in => Some(see_other("/", None)),
            Some(offered) => {
                let mut token = self.token.lock().unwrap_or_else(PoisonError::into_inner);
                if !token
                    .as_deref()
                    .is_some_and(|t| same(t.as_bytes(), offered.as_bytes()))
                {
                    return Some(text(
                        StatusCode::FORBIDDEN,
                        "this link was used already; run `a web` again for a new one",
                    ));
                }
                *token = None;
                let cookie = format!(
                    "{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Strict",
                    self.cookie
                );
                Some(see_other("/", Some(cookie)))
            }
            None if signed_in => None,
            None => Some(text(
                StatusCode::FORBIDDEN,
                "open the link `a web` printed to sign in",
            )),
        }
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

fn text(status: StatusCode, message: &str) -> Response<Vec<u8>> {
    let mut response = Response::new(format!("{message}\n").into_bytes());
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn see_other(location: &'static str, cookie: Option<String>) -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = StatusCode::SEE_OTHER;
    let headers = response.headers_mut();
    headers.insert(LOCATION, HeaderValue::from_static(location));
    if let Some(cookie) = cookie.and_then(|c| HeaderValue::try_from(c).ok()) {
        headers.insert(SET_COOKIE, cookie);
    }
    response
}

/// Serves one connection: a single request, then closes.
pub async fn serve(rt: Rt, router: Router, gate: Arc<Gate>, stream: TcpStream) {
    let stream = Arc::new(stream);
    let _ = respond(&rt, &router, &gate, &stream).await;
    // Also wakes the probe of `closed`, which holds a duplicate.
    let _ = stream.shutdown(Shutdown::Both);
}

async fn respond(rt: &Rt, router: &Router, gate: &Gate, stream: &Arc<TcpStream>) -> io::Result<()> {
    let from = Arc::clone(stream);
    let request = match rt::unblock(&**rt, move || read_request(&from)).await {
        Ok(request) => request,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            return send_whole(rt, stream, text(StatusCode::BAD_REQUEST, &e.to_string())).await;
        }
        Err(e) => return Err(e),
    };
    if let Some(response) = gate.answer(&request) {
        return send_whole(rt, stream, response).await;
    }
    if request.uri().path() == SCRIPT_PATH {
        let mut response = Response::new(SCRIPT.to_vec());
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/javascript"));
        return send_whole(rt, stream, response).await;
    }
    let closed = closed(rt, stream)?;
    let response = router.handle(request.map(Body::from)).await;
    send_streamed(rt, stream, response, &closed).await
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
/// goes away: a response waiting on the daemon would otherwise notice only
/// on its next write.
fn closed(rt: &Rt, stream: &TcpStream) -> io::Result<Receiver<()>> {
    let mut probe = stream.try_clone()?;
    let (tx, rx) = rt::channel::bounded(1);
    rt.spawn_blocking(Box::new(move || {
        // One request per connection: anything read now is the end.
        let _ = probe.read(&mut [0; 1]);
        drop(tx);
    }));
    Ok(rx)
}

fn head(status: StatusCode, headers: &HeaderMap, framing: &str) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status}\r\n").into_bytes();
    for (name, value) in headers {
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    if !headers.contains_key(http::header::CACHE_CONTROL) {
        head.extend_from_slice(b"cache-control: no-store\r\n");
    }
    head.extend_from_slice(
        b"x-content-type-options: nosniff\r\n\
          referrer-policy: same-origin\r\n\
          content-security-policy: frame-ancestors 'none'\r\n\
          connection: close\r\n",
    );
    head.extend_from_slice(framing.as_bytes());
    head.extend_from_slice(b"\r\n\r\n");
    head
}

fn write(rt: &Rt, stream: &Arc<TcpStream>, bytes: Vec<u8>) -> impl Future<Output = io::Result<()>> {
    let stream = Arc::clone(stream);
    rt::unblock(&**rt, move || (&*stream).write_all(&bytes))
}

async fn send_whole(
    rt: &Rt,
    stream: &Arc<TcpStream>,
    response: Response<Vec<u8>>,
) -> io::Result<()> {
    let (parts, body) = response.into_parts();
    let mut bytes = head(
        parts.status,
        &parts.headers,
        &format!("content-length: {}", body.len()),
    );
    bytes.extend_from_slice(&body);
    write(rt, stream, bytes).await
}

/// Writes each frame of the body as one chunk as soon as it is produced,
/// until the body ends or the browser goes away.
async fn send_streamed(
    rt: &Rt,
    stream: &Arc<TcpStream>,
    response: Response<Body>,
    closed: &Receiver<()>,
) -> io::Result<()> {
    let (parts, mut body) = response.into_parts();
    write(
        rt,
        stream,
        head(parts.status, &parts.headers, "transfer-encoding: chunked"),
    )
    .await?;
    loop {
        let next = {
            let mut frame = pin!(body.frame());
            poll_fn(|cx| {
                if closed.poll_recv(cx).is_ready() {
                    return Poll::Ready(None);
                }
                frame.as_mut().poll(cx).map(Some)
            })
            .await
        };
        let frame = match next {
            // The browser left.
            None => return Ok(()),
            Some(None) => break,
            Some(Some(frame)) => frame.map_err(|e| io::Error::other(e.to_string()))?,
        };
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            let mut chunk = format!("{:x}\r\n", data.len()).into_bytes();
            chunk.extend_from_slice(&data);
            chunk.extend_from_slice(b"\r\n");
            write(rt, stream, chunk).await?;
        }
    }
    write(rt, stream, b"0\r\n\r\n".to_vec()).await
}

#[cfg(test)]
mod tests {
    use topcoat::asset::RawAsset;

    use super::SCRIPT;

    /// The embedded browser runtime is the file the linked `topcoat`
    /// declares, found through the declaration it compiles into binaries.
    #[test]
    fn the_embedded_runtime_matches_topcoats() {
        let binary = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let id = topcoat::runtime::SCRIPT.id();
        let declared = RawAsset::find_in_binary(&binary)
            .into_iter()
            .find(|asset| asset.id() == id)
            .expect("topcoat's runtime script is declared in this binary");
        let published = std::fs::read(declared.resolved_path()).unwrap();
        assert!(
            published == SCRIPT,
            "update assets/ from {}",
            declared.resolved_path().display()
        );
    }
}
