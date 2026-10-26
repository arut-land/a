//! One connection: a TCP stream registered with the runtime's reactor,
//! under TLS for https. Every operation waits for readiness on the reactor
//! instead of holding a thread, and dropping a connection closes it.

use std::future::Future;
use std::io::{self, Read as _};
use std::net::{SocketAddr, TcpStream};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use super::proxy::Proxies;
use super::tls;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an attempt runs alone before the next address is tried beside
/// it (RFC 8305's recommended connection attempt delay).
const STAGGER: Duration = Duration::from_millis(250);

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
        let invalid = |what| io::Error::new(io::ErrorKind::InvalidInput, format!("{what}: {uri}"));
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(invalid("not an http(s) URL")),
        };
        let host = uri
            .host()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| invalid("no host"))?;
        Ok(Self {
            https,
            host: host.to_owned(),
            port: uri.port_u16().unwrap_or(if https { 443 } else { 80 }),
        })
    }
}

pub(super) enum Conn {
    Plain(rt::Async<TcpStream>),
    Tls(tls::Stream),
}

impl Conn {
    /// Dials `target`, through its proxy unless it is bypassed, within the
    /// connect timeout.
    pub async fn open(
        rt: &rt::Rt,
        target: &Target,
        proxies: &Proxies,
        tls: &tls::Config,
    ) -> io::Result<Self> {
        let opening = async {
            let proxy = proxies.route(target)?;
            let (host, port) = proxy.map_or((target.host.as_str(), target.port), |proxy| {
                (proxy.host.as_str(), proxy.port)
            });
            let addrs = rt::resolve(&**rt, host, port).await?;
            let socket = dial(rt, interleaved(addrs)).await?;
            socket.get_ref().set_nodelay(true)?;
            sys::net::keep_alive(socket.get_ref())?;
            if let Some(proxy) = proxy {
                proxy.tunnel(&socket, target).await?;
            }
            Ok(if target.https {
                Self::Tls(tls.connect(rt, &target.host, socket).await?)
            } else {
                Self::Plain(socket)
            })
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

    pub async fn read(&mut self, rt: &rt::Rt, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(socket) => socket.read_with(|mut socket| socket.read(buf)).await,
            Self::Tls(tls) => tls.read(rt, buf).await,
        }
    }

    pub async fn write_all(&mut self, rt: &rt::Rt, mut bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(socket) => {
                use std::io::Write as _;
                while !bytes.is_empty() {
                    let n = super::within(
                        rt,
                        socket.write_with(|mut socket| socket.write(bytes)),
                        "sending the request",
                    )
                    .await?;
                    if n == 0 {
                        return Err(io::ErrorKind::WriteZero.into());
                    }
                    bytes = &bytes[n..];
                }
                Ok(())
            }
            Self::Tls(tls) => tls.write_all(rt, bytes).await,
        }
    }

    /// Whether an idle connection can carry another request: the peer
    /// has neither closed it nor sent anything unasked.
    pub fn is_reusable(&self) -> bool {
        let socket = match self {
            Self::Plain(socket) => socket.get_ref(),
            Self::Tls(tls) => tls.socket(),
        };
        matches!(socket.peek(&mut [0]), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
    }
}

/// The resolver's order with the address families alternating from its
/// first choice on (RFC 8305 §4), so an unreachable family costs one
/// stagger rather than all its addresses.
fn interleaved(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first) = addrs.first().map(SocketAddr::is_ipv6) else {
        return addrs;
    };
    let (mut preferred, mut other): (Vec<_>, Vec<_>) =
        addrs.into_iter().partition(|addr| addr.is_ipv6() == first);
    let (mut preferred, mut other) = (preferred.drain(..), other.drain(..));
    let mut order = Vec::with_capacity(preferred.len() + other.len());
    loop {
        match (preferred.next(), other.next()) {
            (None, None) => return order,
            (a, b) => order.extend(a.into_iter().chain(b)),
        }
    }
}

type Attempt = Pin<Box<dyn Future<Output = io::Result<rt::Async<TcpStream>>> + Send>>;

/// Happy Eyeballs (RFC 8305 §5): one attempt at a time in `addrs` order,
/// each next one started when the last fails or after [`STAGGER`],
/// whichever is first; the first to connect wins and the rest are dropped.
async fn dial(rt: &rt::Rt, addrs: Vec<SocketAddr>) -> io::Result<rt::Async<TcpStream>> {
    let reactor = rt.reactor();
    let mut addrs = addrs.into_iter();
    let mut running: Vec<Attempt> = Vec::new();
    let mut stagger: Option<rt::BoxFuture<'static, ()>> = None;
    let mut last = io::Error::from(io::ErrorKind::ConnectionRefused);
    // Start the next attempt at once: at first, and when one fails.
    let mut now = true;
    std::future::poll_fn(|cx| {
        loop {
            let due = std::mem::take(&mut now)
                || stagger
                    .as_mut()
                    .is_some_and(|stagger| stagger.as_mut().poll(cx).is_ready());
            if due {
                stagger = None;
                match addrs.next() {
                    Some(addr) => {
                        let reactor = reactor.clone();
                        running.push(Box::pin(async move {
                            rt::Async::<TcpStream>::connect(&reactor, addr).await
                        }));
                        stagger = Some(rt.sleep(STAGGER));
                    }
                    None if running.is_empty() => {
                        let refused = io::ErrorKind::ConnectionRefused.into();
                        return Poll::Ready(Err(std::mem::replace(&mut last, refused)));
                    }
                    None => {}
                }
            }
            let mut at = 0;
            while at < running.len() {
                match running[at].as_mut().poll(cx) {
                    Poll::Ready(Ok(socket)) => return Poll::Ready(Ok(socket)),
                    Poll::Ready(Err(error)) => {
                        last = error;
                        drop(running.swap_remove(at));
                        now = true;
                    }
                    Poll::Pending => at += 1,
                }
            }
            // A new stagger is polled once to arm it before waiting.
            if !now && !due {
                return Poll::Pending;
            }
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_families_alternate_from_the_resolver_s_first() {
        let v6 = |n: u16| SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, n], 443));
        let v4 = |n: u8| SocketAddr::from(([10, 0, 0, n], 443));
        assert_eq!(
            interleaved(vec![v6(1), v6(2), v6(3), v4(1)]),
            [v6(1), v4(1), v6(2), v6(3)]
        );
        assert_eq!(
            interleaved(vec![v4(1), v4(2), v6(1), v6(2)]),
            [v4(1), v6(1), v4(2), v6(2)]
        );
    }

    /// An address that never answers holds a connection back by one
    /// stagger, not the connect timeout.
    #[test]
    fn an_unanswering_address_delays_the_next_by_one_stagger() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let reachable = listener.local_addr().unwrap();
        // TEST-NET-1 (RFC 5737): nothing answers; a host without a route
        // fails it at once, which is the same as far as the order goes.
        let silent = SocketAddr::from(([192, 0, 2, 1], 9));
        let rt: rt::Rt = std::sync::Arc::new(rt::Threads::new(1));
        let started = std::time::Instant::now();
        let socket = rt::block_on(dial(&rt, vec![silent, reachable])).unwrap();
        assert_eq!(socket.get_ref().peer_addr().unwrap(), reachable);
        assert!(started.elapsed() < STAGGER * 4, "{:?}", started.elapsed());
    }
}
