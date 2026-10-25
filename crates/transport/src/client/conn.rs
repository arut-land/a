//! One connection: a TCP stream registered with the runtime's reactor,
//! under TLS for https. Every operation waits for readiness on the reactor
//! instead of holding a thread, and dropping a connection closes it.

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
use std::io::Write as _;
use std::io::{self, Read as _};
use std::net::{SocketAddr, TcpStream};
#[cfg(feature = "tls-rustls")]
use std::sync::Arc;
#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
use std::sync::OnceLock;
use std::time::Duration;

use super::proxy::Proxy;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(all(feature = "tls-native", feature = "tls-rustls"))]
compile_error!("one TLS provider: tls-native or tls-rustls");

/// Where a request goes: a pool key, and what a new connection dials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub https: bool,
    /// As the URI spells it: an IPv6 literal keeps its brackets.
    pub host: String,
    pub port: u16,
}

impl TryFrom<&http::Uri> for Target {
    type Error = io::Error;

    fn try_from(uri: &http::Uri) -> io::Result<Self> {
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(invalid(format!("not an http(s) URL: {uri}"))),
        };
        let host = uri
            .host()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| invalid(format!("no host in {uri}")))?;
        Ok(Self {
            https,
            host: host.to_owned(),
            port: uri.port_u16().unwrap_or(if https { 443 } else { 80 }),
        })
    }
}

pub(super) enum Conn {
    Plain(rt::Async<TcpStream>),
    #[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
    Tls(Box<Secure>),
}

impl Conn {
    /// Dials `target`, through `proxy` unless it is bypassed, within the
    /// connect timeout.
    pub async fn open(
        rt: &rt::Rt,
        target: &Target,
        proxy: Option<&Proxy>,
        tls: &Tls,
    ) -> io::Result<Self> {
        let opening = async {
            let proxy = proxy.filter(|proxy| proxy.applies_to(&target.host));
            let (host, port) = proxy.map_or((target.host.as_str(), target.port), |proxy| {
                (proxy.host.as_str(), proxy.port)
            });
            let addrs = rt::resolve(&**rt, host, port).await?;
            let socket = dial(rt, &addrs).await?;
            if let Some(proxy) = proxy {
                proxy.tunnel(&socket, target).await?;
            }
            if target.https {
                return tls.wrap(&target.host, socket).await;
            }
            Ok(Self::Plain(socket))
        };
        rt::or(opening, async {
            rt.sleep(CONNECT_TIMEOUT).await;
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out connecting",
            ))
        })
        .await
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(socket) => socket.read_with(|mut socket| socket.read(buf)).await,
            #[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
            Self::Tls(tls) => retry(tls, |tls| tls.read(buf)).await,
        }
    }

    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(socket) => socket.write_all(bytes).await,
            #[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
            Self::Tls(tls) => {
                let mut bytes = bytes;
                while !bytes.is_empty() {
                    match retry(tls, |tls| tls.write(bytes)).await? {
                        0 => return Err(io::ErrorKind::WriteZero.into()),
                        n => bytes = &bytes[n..],
                    }
                }
                retry(tls, io::Write::flush).await
            }
        }
    }

    /// Whether an idle connection can carry another request: the peer
    /// has neither closed it nor sent anything unasked.
    pub fn is_reusable(&self) -> bool {
        let socket = match self {
            Self::Plain(socket) => socket.get_ref(),
            #[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
            Self::Tls(tls) => tls.io().socket.get_ref(),
        };
        matches!(socket.peek(&mut [0]), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
    }
}

/// The first address that answers, each given an equal share of the
/// budget so one that never does leaves time for the next.
async fn dial(rt: &rt::Rt, addrs: &[SocketAddr]) -> io::Result<rt::Async<TcpStream>> {
    let reactor = rt.reactor();
    let each = CONNECT_TIMEOUT / u32::try_from(addrs.len()).unwrap_or(u32::MAX).max(1);
    let mut last = io::Error::from(io::ErrorKind::ConnectionRefused);
    for &addr in addrs {
        let attempt = rt::or(rt::Async::<TcpStream>::connect(&reactor, addr), async {
            rt.sleep(each).await;
            Err(io::ErrorKind::TimedOut.into())
        });
        match attempt.await {
            Ok(socket) => {
                socket.get_ref().set_nodelay(true)?;
                return Ok(socket);
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

pub(super) fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// How TLS reaches the socket: the TLS library's blocking-style calls over
/// a non-blocking socket, which report would-block in the direction the
/// socket refused, so the call waits for that readiness and is retried.
#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
pub(super) struct Io {
    socket: rt::Async<TcpStream>,
    wants: Direction,
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
#[derive(Clone, Copy)]
enum Direction {
    Read,
    Write,
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
impl Io {
    fn new(socket: rt::Async<TcpStream>) -> Self {
        Self {
            socket,
            wants: Direction::Read,
        }
    }

    async fn ready(&self) {
        match self.wants {
            Direction::Read => self.socket.readable().await,
            Direction::Write => self.socket.writable().await,
        }
    }

    fn note<T>(&mut self, wants: Direction, result: io::Result<T>) -> io::Result<T> {
        if matches!(&result, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
            self.wants = wants;
        }
        result
    }
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
impl io::Read for Io {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = self.socket.get_ref().read(buf);
        self.note(Direction::Read, result)
    }
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
impl io::Write for Io {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result = self.socket.get_ref().write(buf);
        self.note(Direction::Write, result)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
async fn retry<R>(
    tls: &mut Secure,
    mut op: impl FnMut(&mut Secure) -> io::Result<R>,
) -> io::Result<R> {
    loop {
        match op(tls) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => tls.io().ready().await,
            result => return result,
        }
    }
}

#[cfg(feature = "tls-native")]
pub(super) type Secure = native_tls::TlsStream<Io>;

#[cfg(feature = "tls-rustls")]
pub(super) type Secure = rustls::StreamOwned<rustls::ClientConnection, Io>;

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
trait Socket {
    fn io(&self) -> &Io;
}

#[cfg(feature = "tls-native")]
impl Socket for Secure {
    fn io(&self) -> &Io {
        self.get_ref()
    }
}

#[cfg(feature = "tls-rustls")]
impl Socket for Secure {
    fn io(&self) -> &Io {
        &self.sock
    }
}

/// The TLS configuration, built on the first https connection: loading
/// trust settings costs memory a plain-http process never needs.
#[derive(Default)]
pub(super) struct Tls {
    #[cfg(feature = "tls-native")]
    config: OnceLock<native_tls::TlsConnector>,
    #[cfg(feature = "tls-rustls")]
    config: OnceLock<Arc<rustls::ClientConfig>>,
}

impl Tls {
    #[cfg(not(any(feature = "tls-native", feature = "tls-rustls")))]
    #[expect(
        clippy::unused_self,
        reason = "the TLS variants keep their configuration"
    )]
    fn wrap(
        &self,
        _: &str,
        _: rt::Async<TcpStream>,
    ) -> impl Future<Output = io::Result<Conn>> + use<> {
        std::future::ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "https needs a TLS feature (tls-native or tls-rustls)",
        )))
    }

    #[cfg(feature = "tls-native")]
    async fn wrap(&self, host: &str, socket: rt::Async<TcpStream>) -> io::Result<Conn> {
        let connector = if let Some(connector) = self.config.get() {
            connector
        } else {
            let connector = native_tls::TlsConnector::new().map_err(io::Error::other)?;
            self.config.get_or_init(|| connector)
        };
        let mut handshake = connector.connect(unbracketed(host), Io::new(socket));
        loop {
            match handshake {
                Ok(tls) => return Ok(Conn::Tls(Box::new(tls))),
                Err(native_tls::HandshakeError::WouldBlock(mid)) => {
                    mid.get_ref().ready().await;
                    handshake = mid.handshake();
                }
                Err(native_tls::HandshakeError::Failure(error)) => {
                    return Err(io::Error::other(error));
                }
            }
        }
    }

    #[cfg(feature = "tls-rustls")]
    async fn wrap(&self, host: &str, socket: rt::Async<TcpStream>) -> io::Result<Conn> {
        let config = if let Some(config) = self.config.get() {
            config
        } else {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .and_then(rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier)
                .map_err(io::Error::other)?
                .with_no_client_auth();
            self.config.get_or_init(|| Arc::new(config))
        };
        let name = rustls::pki_types::ServerName::try_from(unbracketed(host).to_owned())
            .map_err(|error| invalid(format!("{host}: {error}")))?;
        let session =
            rustls::ClientConnection::new(Arc::clone(config), name).map_err(io::Error::other)?;
        let mut tls = rustls::StreamOwned::new(session, Io::new(socket));
        while tls.conn.is_handshaking() {
            match tls.conn.complete_io(&mut tls.sock) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => tls.sock.ready().await,
                Err(e) => return Err(e),
            }
        }
        Ok(Conn::Tls(Box::new(tls)))
    }
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
fn unbracketed(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}
