//! An [`Http`] client over `ureq`: each request runs on the runtime's
//! blocking pool, and the body streams back in chunks through a bounded
//! channel, so a slow reader holds the connection back rather than
//! buffering it. Dropping the reply, before its head or during its body,
//! shuts its connection down: the blocked thread returns at once, and the
//! provider sees the request go away rather than generating on.

use std::cell::RefCell;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout,
    Transport,
};

use crate::{BoxFuture, Http, HttpClients, Payload, RedirectPolicy, Response};

/// How long to wait for a reply's head: a reasoning model may think for
/// minutes before its first byte.
const HEAD_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK: usize = 16 * 1024;
const AHEAD: usize = 16;

#[derive(Clone)]
pub struct Ureq {
    agent: ureq::Agent,
    rt: Arc<dyn rt::Runtime>,
}

enum Piece {
    Head(http::response::Parts),
    Chunk(Vec<u8>),
}

#[cfg(all(feature = "tls-native", feature = "tls-rustls"))]
compile_error!("one TLS provider: tls-native or tls-rustls");

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
const PROVIDER: TlsProvider = if cfg!(feature = "tls-native") {
    TlsProvider::NativeTls
} else {
    TlsProvider::Rustls
};

impl Ureq {
    pub fn new(rt: Arc<dyn rt::Runtime>, redirects: RedirectPolicy) -> Self {
        let config = ureq::Agent::config_builder();
        #[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
        let config = config.tls_config(
            TlsConfig::builder()
                .provider(PROVIDER)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        );
        let config = config
            .http_status_as_error(false)
            .max_redirects(match redirects {
                RedirectPolicy::Refuse => 0,
                RedirectPolicy::Follow => 10,
            })
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(HEAD_TIMEOUT))
            .build();
        // ureq's default chain with its TCP connector swapped for one whose
        // sockets an abandoned request can shut down.
        let connector = ().chain(ConnectProxyConnector::default()).chain(Tcp);
        #[cfg(feature = "tls-native")]
        let connector =
            connector.chain(ureq::unversioned::transport::NativeTlsConnector::default());
        #[cfg(feature = "tls-rustls")]
        let connector = connector.chain(ureq::unversioned::transport::RustlsConnector::default());
        let agent = ureq::Agent::with_parts(config, connector, DefaultResolver::default());
        Self { agent, rt }
    }
}

impl Http for Ureq {
    fn send(&self, request: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>> {
        let (tx, rx) = rt::channel::bounded::<io::Result<Piece>>(AHEAD);
        let agent = self.agent.clone();
        let abort = Arc::new(Abort::default());
        let guard = Guard(Arc::clone(&abort));
        let bytes = request.headers().iter().fold(
            request
                .body()
                .len()
                .saturating_add((AHEAD + 1) * CHUNK + (128 << 10)),
            |bytes, (name, value)| {
                bytes
                    .saturating_add(name.as_str().len())
                    .saturating_add(value.len())
            },
        );
        let admitted = self.rt.spawn_blocking(
            rt::BlockingClass::Network,
            bytes,
            Box::new(move || {
                if abort.line().abandoned {
                    return;
                }
                CURRENT.set(Some(Arc::clone(&abort)));
                run(&agent, request, &tx);
                CURRENT.set(None);
                abort.release();
            }),
        );
        Box::pin(async move {
            admitted.await?;
            let head = rx.recv().await;
            let parts = match head {
                Some(Ok(Piece::Head(parts))) => parts,
                Some(Err(error)) => return Err(error),
                Some(Ok(Piece::Chunk(_))) | None => {
                    return Err(io::Error::other("the connection closed before a reply"));
                }
            };
            let body = chunks(rx, guard);
            Ok(http::Response::from_parts(parts, body))
        })
    }
}

fn run(
    agent: &ureq::Agent,
    request: http::Request<Payload>,
    tx: &rt::channel::BlockingSender<io::Result<Piece>>,
) {
    let (mut parts, body) = request.into_parts();
    parts
        .headers
        .insert(http::header::CONTENT_LENGTH, body.len().into());
    let mut reader = body.reader();
    let body = ureq::SendBody::from_reader(&mut reader);
    let reply = match agent.run(http::Request::from_parts(parts, body)) {
        Ok(reply) => reply,
        Err(error) => {
            let _ = tx.send(Err(io::Error::other(error)));
            return;
        }
    };
    let (parts, body) = reply.into_parts();
    if tx.send(Ok(Piece::Head(parts))).is_err() {
        return;
    }
    let mut reader = body.into_reader();
    // Each chunk is copied out at its size: a buffer handed over whole
    // would hold its full capacity for a few bytes of frame.
    let mut buf = vec![0; CHUNK];
    loop {
        let piece = match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => Ok(Piece::Chunk(buf[..n].to_vec())),
            Err(error) => Err(error),
        };
        let failed = piece.is_err();
        if tx.send(piece).is_err() || failed {
            return;
        }
    }
}

fn chunks(rx: rt::channel::Receiver<io::Result<Piece>>, guard: Guard) -> crate::Body {
    struct Chunks {
        rx: rt::channel::Receiver<io::Result<Piece>>,
        _guard: Guard,
    }

    impl futures_core::Stream for Chunks {
        type Item = io::Result<Vec<u8>>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            self.rx.poll_recv(cx).map(|piece| {
                piece.map(|piece| match piece? {
                    Piece::Chunk(chunk) => Ok(chunk),
                    Piece::Head(_) => Err(io::Error::other("a second reply head")),
                })
            })
        }
    }

    Box::pin(Chunks { rx, _guard: guard })
}

thread_local! {
    /// The request the blocking thread runs: the sockets it touches are
    /// registered with it, whether fresh or from the pool.
    static CURRENT: RefCell<Option<Arc<Abort>>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct Abort(Mutex<Line>);

#[derive(Default)]
struct Line {
    socket: Option<(u64, TcpStream)>,
    /// Whether the caller went away: a socket registered later is shut at
    /// once.
    abandoned: bool,
}

impl Abort {
    fn line(&self) -> std::sync::MutexGuard<'_, Line> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, id: u64, socket: &TcpStream) {
        let mut line = self.line();
        if line.abandoned {
            let _ = socket.shutdown(Shutdown::Both);
        } else if line.socket.as_ref().is_none_or(|(at, _)| *at != id) {
            line.socket = socket.try_clone().ok().map(|handle| (id, handle));
        }
    }

    fn release(&self) {
        self.line().socket = None;
    }

    fn abandon(&self) {
        let mut line = self.line();
        line.abandoned = true;
        if let Some((_, socket)) = line.socket.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

struct Guard(Arc<Abort>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.abandon();
    }
}

#[derive(Debug)]
struct Tcp;

impl<In: Transport> Connector<In> for Tcp {
    type Out = Either<In, Socket>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        if let Some(chained) = chained {
            return Ok(Some(Either::A(chained)));
        }
        // The budget is shared by the addresses, so one that never answers
        // leaves time for the next.
        let count = u32::try_from(details.addrs.len())
            .unwrap_or(u32::MAX)
            .max(1);
        let each = details.timeout.not_zero().map(|t| *t / count);
        let mut last = io::Error::from(io::ErrorKind::ConnectionRefused);
        for addr in &details.addrs {
            let stream = match each {
                Some(each) => TcpStream::connect_timeout(addr, each),
                None => TcpStream::connect(addr),
            };
            match stream {
                Ok(stream) => {
                    stream.set_nodelay(details.config.no_delay())?;
                    let config = details.config;
                    let buffers =
                        LazyBuffers::new(config.input_buffer_size(), config.output_buffer_size());
                    return Ok(Some(Either::B(Socket::new(stream, buffers))));
                }
                Err(e) => last = e,
            }
        }
        Err(match last.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
                ureq::Error::Timeout(ureq::Timeout::Connect)
            }
            _ => last.into(),
        })
    }
}

struct Socket {
    id: u64,
    stream: TcpStream,
    buffers: LazyBuffers,
    read: Option<Duration>,
    write: Option<Duration>,
}

impl Socket {
    fn new(stream: TcpStream, buffers: LazyBuffers) -> Self {
        static IDS: AtomicU64 = AtomicU64::new(0);
        Self {
            id: IDS.fetch_add(1, Ordering::Relaxed),
            stream,
            buffers,
            read: None,
            write: None,
        }
    }

    fn register(&self) {
        CURRENT.with_borrow(|abort| {
            if let Some(abort) = abort {
                abort.register(self.id, &self.stream);
            }
        });
    }
}

fn retime(
    timeout: NextTimeout,
    set: &mut Option<Duration>,
    apply: impl FnOnce(Option<Duration>) -> io::Result<()>,
) -> io::Result<()> {
    let timeout = timeout.not_zero().map(|t| *t);
    if timeout != *set {
        apply(timeout)?;
        *set = timeout;
    }
    Ok(())
}

fn expired(e: io::Error, timeout: NextTimeout) -> ureq::Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ureq::Error::Timeout(timeout.reason),
        _ => e.into(),
    }
}

impl Transport for Socket {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.register();
        let stream = &self.stream;
        retime(timeout, &mut self.write, |t| stream.set_write_timeout(t))?;
        let output = &self.buffers.output()[..amount];
        (&self.stream)
            .write_all(output)
            .map_err(|e| expired(e, timeout))
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.register();
        let stream = &self.stream;
        retime(timeout, &mut self.read, |t| stream.set_read_timeout(t))?;
        let input = self.buffers.input_append_buf();
        let read = (&self.stream)
            .read(input)
            .map_err(|e| expired(e, timeout))?;
        self.buffers.input_appended(read);
        Ok(read > 0)
    }

    fn is_open(&mut self) -> bool {
        let probe = || -> io::Result<bool> {
            self.stream.set_nonblocking(true)?;
            let open = matches!(
                (&self.stream).read(&mut [0]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock
            );
            self.stream.set_nonblocking(false)?;
            Ok(open)
        };
        probe().unwrap_or(false)
    }
}

impl fmt::Debug for Socket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Socket")
            .field("addr", &self.stream.peer_addr().ok())
            .finish_non_exhaustive()
    }
}

pub struct Clients {
    follow: Arc<dyn Http>,
    refuse: Arc<dyn Http>,
}

impl Clients {
    pub fn new(rt: Arc<dyn rt::Runtime>) -> Self {
        Self {
            follow: Arc::new(Ureq::new(Arc::clone(&rt), RedirectPolicy::Follow)),
            refuse: Arc::new(Ureq::new(rt, RedirectPolicy::Refuse)),
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
