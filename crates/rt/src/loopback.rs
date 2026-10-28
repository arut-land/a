//! Ephemeral loopback listeners use the runtime's existing reactor.
use std::io;
use std::net::{Ipv4Addr, TcpListener, TcpStream};

use crate::{Async, Reactor};

/// An owned loopback listener: dropping its owner closes the socket immediately.
pub struct LoopbackListener {
    listener: Async<TcpListener>,
    reactor: Reactor,
    port: u16,
}

impl LoopbackListener {
    /// Binds an unused IPv4 loopback port, never a public interface.
    pub fn bind(reactor: &Reactor) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener: Async::socket(reactor, listener)?,
            reactor: reactor.clone(),
            port,
        })
    }

    /// The ephemeral port assigned by the OS.
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Waits for a connection through the same reactor as the listener.
    pub async fn accept(&self) -> io::Result<Async<TcpStream>> {
        let (stream, _) = self.listener.read_with(TcpListener::accept).await?;
        Async::socket(&self.reactor, stream)
    }
}
