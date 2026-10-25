//! TLS over a non-blocking socket: the TLS library's ordinary calls, whose
//! output lands in a buffer of ours rather than the socket. The library
//! therefore never sees a write would-block (Security.framework queues a
//! refused write whole and reports it sent), and every call is followed by
//! draining that buffer onto the socket in slices as it turns writable. A
//! would-block the library reports is then always a read waiting for the
//! peer. Without a TLS feature an https request fails.

use std::io;
#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
use std::io::{Read as _, Write as _};
use std::net::TcpStream;

/// Plaintext handed to the library at once, and ciphertext written to the
/// socket at once: about one TLS record.
#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
const SLICE: usize = 16 << 10;

#[cfg(all(feature = "tls-native", feature = "tls-rustls"))]
compile_error!("one TLS provider: tls-native or tls-rustls");

#[cfg(feature = "tls-native")]
type Session = native_tls::TlsStream<Io>;
#[cfg(feature = "tls-rustls")]
type Session = rustls::StreamOwned<rustls::ClientConnection, Io>;

/// A TLS session over its socket.
#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
pub(super) struct Stream(Box<Session>);

/// Without a TLS feature no session exists.
#[cfg(not(any(feature = "tls-native", feature = "tls-rustls")))]
pub(super) enum Stream {}

/// The TLS configuration, built on the first https connection: loading
/// trust settings costs memory a plain-http process never needs.
#[derive(Default)]
pub(super) struct Config {
    #[cfg(feature = "tls-native")]
    built: std::sync::OnceLock<native_tls::TlsConnector>,
    #[cfg(feature = "tls-rustls")]
    built: std::sync::OnceLock<std::sync::Arc<rustls::ClientConfig>>,
    /// A certificate the tests' servers present, trusted beside the
    /// platform's.
    #[cfg(all(test, any(feature = "tls-native", feature = "tls-rustls")))]
    pub trusted: Option<Vec<u8>>,
}

#[cfg(not(any(feature = "tls-native", feature = "tls-rustls")))]
impl Config {
    #[expect(clippy::unused_self, reason = "the TLS variants keep their configuration")]
    pub fn connect(
        &self,
        _: &str,
        _: rt::Async<TcpStream>,
    ) -> impl Future<Output = io::Result<Stream>> + use<> {
        std::future::ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "https needs a TLS feature (tls-native or tls-rustls)",
        )))
    }
}

#[cfg(not(any(feature = "tls-native", feature = "tls-rustls")))]
impl Stream {
    pub fn read(&mut self, _: &mut [u8]) -> std::future::Ready<io::Result<usize>> {
        match *self {}
    }

    pub fn write_all(&mut self, _: &[u8]) -> std::future::Ready<io::Result<()>> {
        match *self {}
    }

    pub fn socket(&self) -> &TcpStream {
        match *self {}
    }
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
impl Config {
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "only tests trust a certificate of their own")
    )]
    fn trusted(&self) -> Option<&[u8]> {
        #[cfg(test)]
        return self.trusted.as_deref();
        #[cfg(not(test))]
        None
    }
}

#[cfg(feature = "tls-native")]
impl Config {
    pub async fn connect(&self, host: &str, socket: rt::Async<TcpStream>) -> io::Result<Stream> {
        let connector = if let Some(connector) = self.built.get() {
            connector
        } else {
            let mut builder = native_tls::TlsConnector::builder();
            if let Some(pem) = self.trusted() {
                builder.add_root_certificate(
                    native_tls::Certificate::from_pem(pem).map_err(io::Error::other)?,
                );
            }
            let connector = builder.build().map_err(io::Error::other)?;
            self.built.get_or_init(|| connector)
        };
        let mut handshake = connector.connect(unbracketed(host), Io::new(socket));
        loop {
            match handshake {
                Ok(mut session) => {
                    session.get_mut().drain().await?;
                    return Ok(Stream(Box::new(session)));
                }
                Err(native_tls::HandshakeError::WouldBlock(mut mid)) => {
                    mid.get_mut().drain().await?;
                    mid.get_ref().socket.readable().await;
                    handshake = mid.handshake();
                }
                Err(native_tls::HandshakeError::Failure(error)) => {
                    return Err(io::Error::other(error));
                }
            }
        }
    }
}

#[cfg(feature = "tls-rustls")]
impl Config {
    pub async fn connect(&self, host: &str, socket: rt::Async<TcpStream>) -> io::Result<Stream> {
        use std::sync::Arc;
        let config = if let Some(config) = self.built.get() {
            config
        } else {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let builder = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(io::Error::other)?;
            let builder = match self.trusted() {
                Some(pem) => {
                    use rustls::pki_types::pem::PemObject as _;
                    let mut roots = rustls::RootCertStore::empty();
                    for cert in rustls::pki_types::CertificateDer::pem_slice_iter(pem) {
                        roots
                            .add(cert.map_err(io::Error::other)?)
                            .map_err(io::Error::other)?;
                    }
                    builder.with_root_certificates(roots)
                }
                None => rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(
                    builder,
                )
                .map_err(io::Error::other)?,
            };
            self.built
                .get_or_init(|| Arc::new(builder.with_no_client_auth()))
        };
        handshake(Arc::clone(config), host, socket).await
    }
}

#[cfg(feature = "tls-rustls")]
async fn handshake(
    config: std::sync::Arc<rustls::ClientConfig>,
    host: &str,
    socket: rt::Async<TcpStream>,
) -> io::Result<Stream> {
    let name = rustls::pki_types::ServerName::try_from(unbracketed(host).to_owned())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, format!("{host}: {error}")))?;
    let session = rustls::ClientConnection::new(config, name).map_err(io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(session, Io::new(socket));
    while tls.conn.is_handshaking() {
        let progress = tls.conn.complete_io(&mut tls.sock);
        tls.sock.drain().await?;
        match progress {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => tls.sock.socket.readable().await,
            Err(e) => return Err(e),
        }
    }
    Ok(Stream(Box::new(tls)))
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
impl Stream {
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.run(|tls| tls.read(buf)).await
    }

    pub async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        for mut slice in bytes.chunks(SLICE) {
            while !slice.is_empty() {
                match self.run(|tls| tls.write(slice)).await? {
                    0 => return Err(io::ErrorKind::WriteZero.into()),
                    n => slice = &slice[n..],
                }
            }
        }
        self.run(io::Write::flush).await
    }

    pub fn socket(&self) -> &TcpStream {
        self.io().socket.get_ref()
    }

    /// Runs one library call, sends what it wrote, and waits for the peer
    /// while the call would block reading.
    async fn run<R>(&mut self, mut op: impl FnMut(&mut Session) -> io::Result<R>) -> io::Result<R> {
        loop {
            let result = op(&mut self.0);
            self.io_mut().drain().await?;
            match result {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.io().socket.readable().await;
                }
                result => return result,
            }
        }
    }

    #[cfg(feature = "tls-native")]
    fn io(&self) -> &Io {
        self.0.get_ref()
    }

    #[cfg(feature = "tls-native")]
    fn io_mut(&mut self) -> &mut Io {
        self.0.get_mut()
    }

    #[cfg(feature = "tls-rustls")]
    fn io(&self) -> &Io {
        &self.0.sock
    }

    #[cfg(feature = "tls-rustls")]
    fn io_mut(&mut self) -> &mut Io {
        &mut self.0.sock
    }
}

/// The socket as the TLS library sees it: reads go straight to it, writes
/// to `out` until [`Io::drain`].
#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
pub(super) struct Io {
    socket: rt::Async<TcpStream>,
    out: Vec<u8>,
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
impl Io {
    fn new(socket: rt::Async<TcpStream>) -> Self {
        Self {
            socket,
            out: Vec::new(),
        }
    }

    async fn drain(&mut self) -> io::Result<()> {
        let Self { socket, out } = self;
        let mut sent = 0;
        while sent < out.len() {
            let end = out.len().min(sent + SLICE);
            match socket
                .write_with(|mut socket| socket.write(&out[sent..end]))
                .await?
            {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                n => sent += n,
            }
        }
        out.clear();
        Ok(())
    }
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
impl io::Read for Io {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket.get_ref().read(buf)
    }
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
impl io::Write for Io {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
fn unbracketed(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}
