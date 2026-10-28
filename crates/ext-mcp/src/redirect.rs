//! OAuth callback framing stays with the extension; rt owns socket readiness.
use std::io::{self, Read};

pub(super) struct Redirect {
    listener: rt::LoopbackListener,
}

impl Redirect {
    pub fn bind(reactor: &rt::Reactor) -> io::Result<Self> {
        Ok(Self {
            listener: rt::LoopbackListener::bind(reactor)?,
        })
    }
    pub fn uri(&self) -> String {
        format!("http://127.0.0.1:{}/oauth/callback", self.listener.port())
    }
    /// Returns a callback query. The caller validates issuer and state before exchange.
    pub async fn receive(&self) -> io::Result<String> {
        loop {
            let stream = self.listener.accept().await?;
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
