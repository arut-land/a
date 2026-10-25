//! An [`Http`] client that never holds a thread: HTTP/1.1 (ureq's sans-IO
//! protocol, `ureq-proto`) over sockets registered with the runtime's
//! reactor, so a request and its streamed reply are plain futures polled by
//! whichever task awaits them. A reply's body is read from the socket only
//! as the caller pulls it, so a slow reader holds the peer back rather than
//! buffering it. Dropping the reply, before its head or during its body,
//! closes its connection at once: the provider sees the request go away
//! rather than generating on. A body read to its end returns the
//! connection to the pool.

mod conn;
mod pool;
mod proxy;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use http::HeaderValue;
use http::header::{ACCEPT, CONTENT_LENGTH, USER_AGENT};
use ureq_proto::BodyMode;
use ureq_proto::client::state::{Prepare, RecvBody, RecvResponse, Redirect, SendRequest};
use ureq_proto::client::{
    Await100Result, Call, RecvBodyResult, RecvResponseResult, RedirectAuthHeaders,
    SendRequestResult,
};

use crate::{BoxFuture, Http, HttpClients, Payload, RedirectPolicy, Response};
use conn::{Conn, Target, Tls};
use pool::Pool;
use proxy::Proxy;

/// How long to wait for a reply's head: a reasoning model may think for
/// minutes before its first byte.
const HEAD_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_REDIRECTS: usize = 10;
/// Reads and decoded chunks are at most this; a reply head may grow the
/// read buffer to [`MAX_HEAD`].
const CHUNK: usize = 16 << 10;
const MAX_HEAD: usize = 64 << 10;

#[derive(Clone)]
pub struct Client(Arc<Shared>);

struct Shared {
    rt: rt::Rt,
    redirects: RedirectPolicy,
    pool: Pool,
    setup: Arc<Setup>,
}

/// What every client of a process shares: read or built once.
struct Setup {
    /// A proxy the environment names but this client cannot use fails
    /// every request rather than being bypassed.
    proxy: Result<Option<Proxy>, (io::ErrorKind, String)>,
    tls: Tls,
}

impl Setup {
    fn from_env() -> Arc<Self> {
        Arc::new(Self {
            proxy: Proxy::from_env().map_err(|error| (error.kind(), error.to_string())),
            tls: Tls::default(),
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
    Redirect(Box<Call<Redirect>>),
}

impl Shared {
    async fn send(self: &Arc<Self>, request: http::Request<Payload>) -> io::Result<Response> {
        let (mut parts, payload) = request.into_parts();
        parts.headers.insert(CONTENT_LENGTH, payload.len().into());
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
        let mut call = Call::new(http::Request::from_parts(parts, ())).map_err(protocol)?;
        // Only the first request carries a body: a redirect that would
        // resend one is refused by the protocol.
        let mut payload = payload;
        let mut redirects = 0;
        loop {
            let mut redirect = match self.exchange(call, std::mem::take(&mut payload)).await? {
                Exchange::Reply(reply) => return Ok(reply),
                Exchange::Redirect(redirect) => redirect,
            };
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return Err(io::Error::other("too many redirects"));
            }
            call = redirect
                .as_new_call(RedirectAuthHeaders::Never)
                .map_err(protocol)?
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "a {} redirect would resend the request body",
                        redirect.status()
                    ))
                })?;
        }
    }

    async fn exchange(
        self: &Arc<Self>,
        call: Call<Prepare>,
        payload: Payload,
    ) -> io::Result<Exchange> {
        let follow = matches!(self.redirects, RedirectPolicy::Follow);
        let target = Target::try_from(call.uri())?;
        let conn = if let Some(conn) = self.pool.take(&target, self.rt.now()) {
            conn
        } else {
            let proxy = self
                .setup
                .proxy
                .as_ref()
                .map_err(|(kind, message)| io::Error::new(*kind, message.clone()))?;
            Conn::open(&self.rt, &target, proxy.as_ref(), &self.setup.tls).await?
        };
        let mut wire = Wire::new(conn);
        let call = wire.send(call.proceed(), &payload).await?;
        let (head, next) = rt::or(wire.head(call, follow), async {
            self.rt.sleep(HEAD_TIMEOUT).await;
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for a reply",
            ))
        })
        .await?;
        let call = match next {
            RecvResponseResult::RecvBody(call) => call,
            RecvResponseResult::Redirect(call) => {
                self.release(target, wire, call.must_close_connection());
                return Ok(if follow {
                    Exchange::Redirect(Box::new(call))
                } else {
                    Exchange::Reply(http::Response::from_parts(head, crate::body([])))
                });
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
            call: Some(call),
            wire: Some(wire),
            closed: false,
            redirect: None,
            target,
            shared: Arc::clone(self),
        };
        if follow && head.status.is_redirection() {
            while reader.next().await?.is_some() {}
            if let Some(redirect) = reader.redirect.take() {
                return Ok(Exchange::Redirect(Box::new(redirect)));
            }
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

/// A connection with its read buffer, for one exchange: an idle pooled
/// connection holds no buffer.
struct Wire {
    conn: Conn,
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl Wire {
    fn new(conn: Conn) -> Self {
        Self {
            conn,
            buf: vec![0; CHUNK],
            start: 0,
            end: 0,
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
                    "the reply head is too large",
                ));
            }
        }
        let n = self.conn.read(&mut self.buf[self.end..]).await?;
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
            self.conn.write_all(&self.buf[..n]).await?;
        }
        let call = match call
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
        let mut call = call;
        // The body has a length, so its pieces go to the socket as they
        // are, never copied into one buffer.
        for piece in payload.pieces() {
            call.consume_direct_write(piece.len()).map_err(protocol)?;
            self.conn.write_all(piece).await?;
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
            if self.fill().await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the connection closed before a reply",
                ));
            }
        }
    }
}

/// A reply body, decoded as the caller pulls it.
struct Reader {
    /// Until the body ends or fails.
    call: Option<Call<RecvBody>>,
    wire: Option<Wire>,
    /// The peer closed its end.
    closed: bool,
    /// Where a redirect's body, read to its end, points.
    redirect: Option<Call<Redirect>>,
    target: Target,
    shared: Arc<Shared>,
}

impl Reader {
    async fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let (Some(call), Some(wire)) = (self.call.as_mut(), self.wire.as_mut()) else {
                return Ok(None);
            };
            let complete = match call.body_mode() {
                BodyMode::NoBody => true,
                BodyMode::LengthDelimited(_) | BodyMode::Chunked => call.can_proceed(),
                BodyMode::CloseDelimited => false,
            };
            if complete {
                self.finish()?;
                return Ok(None);
            }
            if !wire.input().is_empty() {
                // Decoding never lengthens what it reads, so a chunk sized
                // to its input holds no more than a few bytes of framing.
                let mut chunk = vec![0; wire.input().len()];
                let (used, n) = call.read(wire.input(), &mut chunk).map_err(protocol)?;
                wire.consume(used);
                if n > 0 {
                    chunk.truncate(n);
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
        let call = self.call.take().expect("a body being read");
        let wire = self.wire.take().expect("a body being read");
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
            RecvBodyResult::Redirect(redirect) => {
                let close = redirect.must_close_connection();
                self.redirect = Some(redirect);
                close
            }
            RecvBodyResult::Cleanup(cleanup) => cleanup.must_close_connection(),
        };
        self.shared
            .release(self.target.clone(), wire, close || self.closed);
        Ok(())
    }

    fn into_body(self) -> crate::Body {
        Box::pin(futures_lite::stream::unfold(
            self,
            |mut reader| async move {
                match reader.next().await {
                    Ok(Some(chunk)) => Some((Ok(chunk), reader)),
                    Ok(None) => None,
                    Err(error) => {
                        // A failed body ends here, and its connection with it.
                        (reader.call, reader.wire) = (None, None);
                        Some((Err(error), reader))
                    }
                }
            },
        ))
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
