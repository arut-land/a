//! An [`Http`] client that never holds a thread: HTTP/1.1 (ureq's sans-IO
//! protocol, `ureq-proto`) over sockets registered with the runtime's
//! reactor, so a request and its streamed reply are plain futures polled by
//! whichever task awaits them. A reply's body is read from the socket only
//! as the caller pulls it, so a slow reader holds the peer back rather than
//! buffering it. Dropping the reply, before its head or during its body,
//! closes its connection at once: the provider sees the request go away
//! rather than generating on. The chunk that completes a body returns the
//! connection to the pool.

mod conn;
mod pool;
mod proxy;
mod tls;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use http::header::{
    ACCEPT, AUTHORIZATION, CONTENT_LENGTH, COOKIE, HeaderName, LOCATION, PROXY_AUTHORIZATION,
    USER_AGENT,
};
use http::{HeaderValue, Method, StatusCode};
use ureq_proto::BodyMode;
use ureq_proto::client::state::{RecvBody, RecvResponse, SendRequest};
use ureq_proto::client::{
    Await100Result, Call, RecvBodyResult, RecvResponseResult, SendRequestResult,
};

use crate::{BoxFuture, Http, HttpClients, Payload, RedirectPolicy, Response};
use conn::{Conn, Target};
use pool::Pool;
use proxy::Proxies;

/// How long the peer may go silent: before a reply's head (a reasoning
/// model may think for minutes before its first byte), between a body's
/// chunks, and while it accepts none of a request. Tests wait seconds.
const IDLE: Duration = if cfg!(test) {
    Duration::from_secs(3)
} else {
    Duration::from_secs(300)
};
const MAX_REDIRECTS: usize = 10;
/// The read buffer an exchange starts with; a reply head may grow it to
/// [`MAX_HEAD`].
const CHUNK: usize = 16 << 10;
const MAX_HEAD: usize = 64 << 10;
/// Credentials the provider codecs send, which a redirect to another host
/// must not carry (`Authorization` never follows a redirect).
const KEY_HEADERS: [&str; 3] = ["x-api-key", "x-goog-api-key", "api-key"];

#[derive(Clone)]
pub struct Client(Arc<Shared>);

struct Shared {
    rt: rt::Rt,
    redirects: RedirectPolicy,
    pool: Pool,
    setup: Arc<Setup>,
}

/// What every client of a process shares: read or built once.
#[derive(Default)]
struct Setup {
    proxies: Proxies,
    tls: tls::Config,
}

impl Setup {
    fn from_env() -> Arc<Self> {
        Arc::new(Self {
            proxies: Proxies::from_env(),
            tls: tls::Config::default(),
        })
    }
}

impl Client {
    pub fn new(rt: rt::Rt, redirects: RedirectPolicy) -> Self {
        Self::with(rt, redirects, Setup::from_env())
    }

    fn with(rt: rt::Rt, redirects: RedirectPolicy, setup: Arc<Setup>) -> Self {
        Self(Arc::new(Shared {
            rt,
            redirects,
            pool: Pool::default(),
            setup,
        }))
    }
}

impl Http for Client {
    fn send(&self, request: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>> {
        let shared = Arc::clone(&self.0);
        Box::pin(async move { shared.send(request).await })
    }
}

enum Exchange {
    Reply(Response),
    Redirect {
        status: StatusCode,
        location: HeaderValue,
    },
}

impl Shared {
    async fn send(self: &Arc<Self>, request: http::Request<Payload>) -> io::Result<Response> {
        let (mut parts, mut payload) = request.into_parts();
        parts
            .headers
            .entry(USER_AGENT)
            .or_insert(HeaderValue::from_static(concat!(
                "a/",
                env!("CARGO_PKG_VERSION")
            )));
        parts
            .headers
            .entry(ACCEPT)
            .or_insert(HeaderValue::from_static("*/*"));
        for _ in 0..=MAX_REDIRECTS {
            // Only the first request carries a body: a redirect that would
            // resend one is refused.
            parts = match self.exchange(&parts, std::mem::take(&mut payload)).await? {
                Exchange::Reply(reply) => return Ok(reply),
                Exchange::Redirect { status, location } => redirected(parts, status, &location)?,
            };
        }
        Err(io::Error::other("too many redirects"))
    }

    async fn exchange(
        self: &Arc<Self>,
        parts: &http::request::Parts,
        payload: Payload,
    ) -> io::Result<Exchange> {
        let follow = matches!(self.redirects, RedirectPolicy::Follow);
        let mut parts = parts.clone();
        parts
            .headers
            .insert(CONTENT_LENGTH, payload.len().into());
        let target = Target::try_from(&parts.uri)?;
        let call = Call::new(http::Request::from_parts(parts, ())).map_err(protocol)?;
        let conn = if let Some(conn) = self.pool.take(&target, self.rt.now()) {
            conn
        } else {
            Conn::open(&self.rt, &target, &self.setup.proxies, &self.setup.tls).await?
        };
        let mut wire = Wire::new(conn, Arc::clone(&self.rt));
        let call = wire.send(call.proceed(), &payload).await?;
        let (head, next) = wire.head(call, follow).await?;
        let redirect = |head: &http::response::Parts| {
            let location = head.headers.get(LOCATION).cloned().ok_or_else(|| {
                io::Error::other(format!("a {} redirect without a location", head.status))
            })?;
            Ok(Exchange::Redirect {
                status: head.status,
                location,
            })
        };
        let call = match next {
            RecvResponseResult::RecvBody(call) => call,
            RecvResponseResult::Redirect(call) => {
                self.release(target, wire, call.must_close_connection());
                return if follow {
                    redirect(&head)
                } else {
                    Ok(Exchange::Reply(http::Response::from_parts(
                        head,
                        crate::body([]),
                    )))
                };
            }
            RecvResponseResult::Cleanup(call) => {
                self.release(target, wire, call.must_close_connection());
                return Ok(Exchange::Reply(http::Response::from_parts(
                    head,
                    crate::body([]),
                )));
            }
        };
        let mut reader = Reader {
            body: Some((call, wire)),
            closed: false,
            target,
            shared: Arc::clone(self),
        };
        if follow && head.status.is_redirection() && head.status != StatusCode::NOT_MODIFIED {
            while reader.next().await?.is_some() {}
            return redirect(&head);
        }
        Ok(Exchange::Reply(http::Response::from_parts(
            head,
            reader.into_body(),
        )))
    }

    /// Pools a connection whose exchange ended cleanly, closes any other.
    fn release(&self, target: Target, wire: Wire, close: bool) {
        if !close && wire.input().is_empty() {
            self.pool.put(target, wire.conn, self.rt.now());
        }
    }
}

/// The request a redirect asks for, by curl's rules: 307 and 308 keep the
/// method but never resend a body, the others become GET (HEAD stays).
/// Credentials stay behind: `Authorization` and cookies always, the
/// providers' key headers when the host changes.
fn redirected(
    mut parts: http::request::Parts,
    status: StatusCode,
    location: &HeaderValue,
) -> io::Result<http::request::Parts> {
    let location = location
        .to_str()
        .map_err(|_| io::Error::other("a redirect to a location that is not text"))?;
    let uri = resolve(&parts.uri, location)?;
    parts.method = match status {
        StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {
            if matches!(
                parts.method,
                Method::POST | Method::PUT | Method::PATCH | Method::DELETE
            ) {
                return Err(io::Error::other(format!(
                    "a {status} redirect would resend the request body"
                )));
            }
            parts.method
        }
        _ if parts.method == Method::HEAD => Method::HEAD,
        _ => Method::GET,
    };
    for name in [AUTHORIZATION, PROXY_AUTHORIZATION, COOKIE, CONTENT_LENGTH] {
        parts.headers.remove(name);
    }
    if uri.host() != parts.uri.host() {
        for name in KEY_HEADERS {
            parts.headers.remove(HeaderName::from_static(name));
        }
    }
    parts.uri = uri;
    Ok(parts)
}

/// `location` against the URI that answered with it.
fn resolve(base: &http::Uri, location: &str) -> io::Result<http::Uri> {
    let scheme = base.scheme_str().unwrap_or("https");
    let authority = base.authority().map_or("", http::uri::Authority::as_str);
    let absolute = if location.contains("://") {
        location.to_owned()
    } else if let Some(rest) = location.strip_prefix("//") {
        format!("{scheme}://{rest}")
    } else if location.starts_with('/') {
        format!("{scheme}://{authority}{location}")
    } else {
        let path = base.path();
        let directory = &path[..path.rfind('/').map_or(0, |at| at + 1)];
        format!("{scheme}://{authority}{directory}{location}")
    };
    absolute
        .parse()
        .map_err(|_| io::Error::other(format!("a redirect to {location}, not a URL")))
}

/// A connection with its buffers, for one exchange: an idle pooled
/// connection holds none.
struct Wire {
    conn: Conn,
    rt: rt::Rt,
    buf: Vec<u8>,
    start: usize,
    end: usize,
    /// Where a body decodes to before its chunk is copied out at size.
    out: Vec<u8>,
}

impl Wire {
    fn new(conn: Conn, rt: rt::Rt) -> Self {
        Self {
            conn,
            rt,
            buf: vec![0; CHUNK],
            start: 0,
            end: 0,
            out: Vec::new(),
        }
    }

    fn input(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    fn consume(&mut self, n: usize) {
        self.start += n;
        if self.start == self.end {
            (self.start, self.end) = (0, 0);
        }
    }

    /// Reads more input after what is buffered; 0 when the peer closed.
    async fn fill(&mut self) -> io::Result<usize> {
        if self.end == self.buf.len() {
            if self.start > 0 {
                self.buf.copy_within(self.start..self.end, 0);
                (self.start, self.end) = (0, self.end - self.start);
            } else if self.buf.len() < MAX_HEAD {
                self.buf.resize(self.buf.len() * 2, 0);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a reply head or chunk line longer than 64 KiB",
                ));
            }
        }
        let Self {
            conn, rt, buf, end, ..
        } = self;
        let n = within(rt, conn.read(&mut buf[*end..]), "waiting for the reply").await?;
        self.end += n;
        Ok(n)
    }

    async fn send(
        &mut self,
        mut call: Call<SendRequest>,
        payload: &Payload,
    ) -> io::Result<Call<RecvResponse>> {
        // Nothing is read before the request is sent, so the read buffer
        // holds the head on its way out.
        while !call.can_proceed() {
            let n = call.write(&mut self.buf).map_err(protocol)?;
            let head = self.conn.write_all(&self.buf[..n]);
            within(&self.rt, head, "sending the request").await?;
        }
        let mut call = match call
            .proceed()
            .map_err(protocol)?
            .expect("the head is written")
        {
            SendRequestResult::Await100(call) => match call.proceed().map_err(protocol)? {
                Await100Result::SendBody(call) => call,
                Await100Result::RecvResponse(call) => return Ok(call),
            },
            SendRequestResult::SendBody(call) => call,
            SendRequestResult::RecvResponse(call) => return Ok(call),
        };
        // The body has a length, so its pieces go to the socket as they
        // are, never copied into one buffer.
        for piece in payload.pieces() {
            call.consume_direct_write(piece.len()).map_err(protocol)?;
            within(&self.rt, self.conn.write_all(piece), "sending the request").await?;
        }
        call.proceed()
            .ok_or_else(|| io::Error::other("the request body is shorter than its length"))
    }

    async fn head(
        &mut self,
        mut call: Call<RecvResponse>,
        follow: bool,
    ) -> io::Result<(http::response::Parts, RecvResponseResult)> {
        loop {
            // Following redirects, a `Location` is enough: some servers
            // never finish a redirect's head.
            let (used, reply) = call.try_response(self.input(), follow).map_err(protocol)?;
            self.consume(used);
            if let Some(reply) = reply {
                let next = call.proceed().expect("a parsed reply proceeds");
                return Ok((reply.into_parts().0, next));
            }
            // What a 1xx left buffered may already hold the reply.
            if used > 0 {
                continue;
            }
            if self.fill().await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the connection closed before a reply",
                ));
            }
        }
    }

    /// Decodes what is buffered: the input it used, and the body bytes.
    fn decode(&mut self, call: &mut Call<RecvBody>) -> io::Result<(usize, Vec<u8>)> {
        let Self {
            buf,
            start,
            end,
            out,
            ..
        } = self;
        let input = &buf[*start..*end];
        // Decoding never lengthens its input, so this much room takes all
        // of it at once, a body's end included.
        if out.len() < input.len() {
            out.resize(input.len(), 0);
        }
        let (used, n) = call.read(input, out).map_err(protocol)?;
        let chunk = out[..n].to_vec();
        self.consume(used);
        Ok((used, chunk))
    }
}

/// `operation`, unless the peer goes silent for [`IDLE`] first.
async fn within<T>(
    rt: &rt::Rt,
    operation: impl Future<Output = io::Result<T>>,
    what: &str,
) -> io::Result<T> {
    rt::or(operation, async {
        rt.sleep(IDLE).await;
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out {what}"),
        ))
    })
    .await
}

/// A reply body, decoded as the caller pulls it.
struct Reader {
    /// Until the body ends or fails.
    body: Option<(Call<RecvBody>, Wire)>,
    /// The peer closed its end.
    closed: bool,
    target: Target,
    shared: Arc<Shared>,
}

impl Reader {
    async fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let Some((call, wire)) = self.body.as_mut() else {
                return Ok(None);
            };
            if complete(call) {
                self.finish()?;
                return Ok(None);
            }
            if !wire.input().is_empty() {
                let (used, chunk) = wire.decode(call)?;
                if !chunk.is_empty() {
                    // A caller that stops at the last event, as a model
                    // stream does, still leaves the connection for the next.
                    if complete(call) {
                        self.finish()?;
                    }
                    return Ok(Some(chunk));
                }
                if used > 0 {
                    continue;
                }
            }
            if self.closed {
                self.finish()?;
                return Ok(None);
            }
            match wire.fill().await {
                Ok(0) => self.closed = true,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    self.closed = true;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        let (call, wire) = self.body.take().expect("a body being read");
        if !call.can_proceed() {
            // `0\r\n` then the peer's close: the whole body arrived, but
            // the connection is done.
            if call.is_ended_chunked() {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the connection closed before the reply's end",
            ));
        }
        let close = match call.proceed().expect("a complete body proceeds") {
            RecvBodyResult::Redirect(redirect) => redirect.must_close_connection(),
            RecvBodyResult::Cleanup(cleanup) => cleanup.must_close_connection(),
        };
        self.shared
            .release(self.target.clone(), wire, close || self.closed);
        Ok(())
    }

    fn into_body(self) -> crate::Body {
        Box::pin(futures_lite::stream::unfold(self, |mut reader| async move {
            match reader.next().await {
                Ok(Some(chunk)) => Some((Ok(chunk), reader)),
                Ok(None) => None,
                Err(error) => {
                    // A failed body ends here, and its connection with it.
                    reader.body = None;
                    Some((Err(error), reader))
                }
            }
        }))
    }
}

/// Whether the body has all arrived: a close-delimited one never has
/// until the peer closes.
fn complete(call: &Call<RecvBody>) -> bool {
    match call.body_mode() {
        BodyMode::NoBody => true,
        BodyMode::LengthDelimited(_) | BodyMode::Chunked => call.can_proceed(),
        BodyMode::CloseDelimited => false,
    }
}

fn protocol(error: ureq_proto::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// The two clients a process needs, sharing their proxy and TLS setup.
pub struct Clients {
    follow: Arc<dyn Http>,
    refuse: Arc<dyn Http>,
}

impl Clients {
    pub fn new(rt: rt::Rt) -> Self {
        let setup = Setup::from_env();
        Self {
            follow: Arc::new(Client::with(
                Arc::clone(&rt),
                RedirectPolicy::Follow,
                Arc::clone(&setup),
            )),
            refuse: Arc::new(Client::with(rt, RedirectPolicy::Refuse, setup)),
        }
    }
}

impl HttpClients for Clients {
    fn client(&self, policy: RedirectPolicy) -> Arc<dyn Http> {
        Arc::clone(match policy {
            RedirectPolicy::Follow => &self.follow,
            RedirectPolicy::Refuse => &self.refuse,
        })
    }
}

#[cfg(test)]
mod tests;
