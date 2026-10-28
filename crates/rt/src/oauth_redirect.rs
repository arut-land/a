//! Ephemeral OAuth loopback callbacks use the runtime's existing reactor.
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};

use crate::{Async, Reactor};

/// An owned listener: cancelling authorization closes its socket immediately.
pub struct OAuthRedirect {
    listener: Async<TcpListener>,
    reactor: Reactor,
    port: u16,
}

impl OAuthRedirect {
    /// Binds an unused IPv4 loopback port, never a public interface.
    pub fn bind(reactor: &Reactor) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener: Async::socket(reactor, listener)?,
            reactor: reactor.clone(),
            port,
        })
    }

    /// The redirect URI advertised to the authorization server.
    pub fn uri(&self) -> String {
        format!("http://127.0.0.1:{}/oauth/callback", self.port)
    }

    /// Returns a callback query. The caller validates issuer and state before exchange.
    pub async fn receive(&self) -> io::Result<String> {
        loop {
            let (stream, _) = self.listener.read_with(TcpListener::accept).await?;
            let stream: Async<TcpStream> = Async::socket(&self.reactor, stream)?;
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let count = stream
                    .read_with(|mut stream| stream.read(&mut buffer))
                    .await?;
                if count == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            let head = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
            let query = head
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("GET /oauth/callback?"))
                .and_then(|line| line.strip_suffix(" HTTP/1.1"));
            if let Some(query) = query {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 37\r\nConnection: close\r\n\r\nReturn to a to finish authorization.\n").await?;
                return Ok(query.to_owned());
            }
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
        }
    }
}
