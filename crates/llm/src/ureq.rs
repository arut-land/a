//! An [`Http`] client over `ureq`: each request runs on the runtime's
//! blocking pool, and the body streams back in chunks through a bounded
//! channel, so a slow reader holds the connection back rather than
//! buffering it. Dropping the body stops the read.

use std::io::{self, Read};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::http::{BoxFuture, Http, Response};

/// How long to wait for a reply's head: a reasoning model may think for
/// minutes before its first byte.
const HEAD_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK: usize = 16 * 1024;
/// Chunks read ahead of the consumer.
const AHEAD: usize = 16;

#[derive(Clone)]
pub struct Ureq {
    agent: ureq::Agent,
    rt: Arc<dyn rt::Runtime>,
}

enum Piece {
    Head(http::response::Parts),
    Chunk(Bytes),
}

impl Ureq {
    pub fn new(rt: Arc<dyn rt::Runtime>) -> Self {
        // The OS's TLS on Apple platforms and Windows; rustls elsewhere.
        let provider = if cfg!(any(target_vendor = "apple", windows)) {
            TlsProvider::NativeTls
        } else {
            TlsProvider::Rustls
        };
        let tls = TlsConfig::builder()
            .provider(provider)
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(HEAD_TIMEOUT))
            .build()
            .new_agent();
        Self { agent, rt }
    }
}

impl Http for Ureq {
    fn send(&self, request: http::Request<Bytes>) -> BoxFuture<'static, io::Result<Response>> {
        let (tx, rx) = rt::channel::bounded::<io::Result<Piece>>(AHEAD);
        let agent = self.agent.clone();
        self.rt.spawn_blocking(Box::new(move || {
            let (parts, body) = request.into_parts();
            let reply = match agent.run(http::Request::from_parts(parts, body.to_vec())) {
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
            loop {
                let mut buf = vec![0; CHUNK];
                let piece = match reader.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(Piece::Chunk(Bytes::from(buf)))
                    }
                    Err(error) => Err(error),
                };
                let failed = piece.is_err();
                // A receiver that went away wants no more of the body.
                if tx.send(piece).is_err() || failed {
                    return;
                }
            }
        }));
        Box::pin(async move {
            let head = std::future::poll_fn(|cx| rx.poll_recv(cx)).await;
            let parts = match head {
                Some(Ok(Piece::Head(parts))) => parts,
                Some(Err(error)) => return Err(error),
                Some(Ok(Piece::Chunk(_))) | None => {
                    return Err(io::Error::other("the connection closed before a reply"));
                }
            };
            let body = futures_stream(rx);
            Ok(http::Response::from_parts(parts, body))
        })
    }
}

/// The chunks after the head, as a body.
fn futures_stream(rx: rt::channel::Receiver<io::Result<Piece>>) -> crate::http::Body {
    struct Chunks(rt::channel::Receiver<io::Result<Piece>>);

    impl futures_core::Stream for Chunks {
        type Item = io::Result<Bytes>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            self.0.poll_recv(cx).map(|piece| {
                piece.map(|piece| match piece? {
                    Piece::Chunk(chunk) => Ok(chunk),
                    Piece::Head(_) => Err(io::Error::other("a second reply head")),
                })
            })
        }
    }

    Box::pin(Chunks(rx))
}
