use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::pin::Pin;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;

/// Well past a socket close's wake-up, well short of any timeout.
const PROMPT: Duration = Duration::from_millis(500);

/// [`rt::Threads`] without a blocking lane: a request that took a
/// blocking worker would panic.
struct Unblocked(rt::Threads);

impl rt::Runtime for Unblocked {
    fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
        self.0.spawn(task);
    }

    fn spawn_blocking(
        &self,
        _: rt::BlockingClass,
        _: usize,
        _: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, io::Result<()>> {
        panic!("a network call took a blocking worker");
    }

    fn sleep(&self, duration: Duration) -> rt::BoxFuture<'static, ()> {
        self.0.sleep(duration)
    }

    fn spawn_flush(
        &self,
        work: Box<dyn FnOnce() + Send>,
    ) -> rt::BoxFuture<'static, io::Result<()>> {
        self.0.spawn_flush(work)
    }

    fn reactor(&self) -> rt::Reactor {
        self.0.reactor()
    }
}

fn client() -> Client {
    Client::new(
        Arc::new(Unblocked(rt::Threads::new(1))),
        RedirectPolicy::Follow,
    )
}

fn request(port: u16) -> http::Request<Payload> {
    http::Request::post(format!("http://127.0.0.1:{port}/v1/responses"))
        .body(b"{}".to_vec().into())
        .unwrap()
}

/// Reads one request's head and body off `reader`.
fn read_request(reader: &mut impl BufRead) -> String {
    let mut headers = String::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        if line == "\r\n" {
            break;
        }
        headers.push_str(&line.to_ascii_lowercase());
    }
    // Closing with unread request bytes can reset TCP and discard the reply.
    reader.read_exact(&mut vec![0; length]).unwrap();
    headers
}

/// A server for one connection: it reads a request, says so on `got`,
/// writes `reply`, then reads until the client closes the connection and
/// says so on `closed`.
fn server(reply: &'static [u8]) -> (u16, rt::oneshot::Receiver<()>, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (got, got_rx) = rt::oneshot::channel();
    let (closed, closed_rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        read_request(&mut reader);
        (&stream).write_all(reply).unwrap();
        got.send(());
        // EOF or a reset: either way the client is gone.
        let _ = reader.read_to_end(&mut Vec::new());
        let _ = closed.send(());
    });
    (port, got_rx, closed_rx)
}

fn next_chunk(body: &mut crate::Body) -> Option<io::Result<Vec<u8>>> {
    rt::block_on(std::future::poll_fn(|cx| {
        futures_core::Stream::poll_next(Pin::as_mut(body), cx)
    }))
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
#[test]
fn https_starts_tls_without_a_bundled_certificate_store() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut header = [0; 3];
        stream.read_exact(&mut header).unwrap();
        assert_eq!(&header[..2], &[22, 3], "a TLS handshake record");
    });
    let request = http::Request::get(format!("https://{address}/"))
        .body(Payload::default())
        .unwrap();
    // The peer closes during TLS: an ordinary transport error, never a feature panic.
    assert!(rt::block_on(client().send(request)).is_err());
    peer.join().unwrap();
}

#[test]
fn dropping_a_request_waiting_for_its_head_closes_the_connection() {
    let (port, got, closed) = server(b"");
    let mut reply = client().send(request(port));
    rt::block_on(rt::or(
        async {
            let _ = (&mut reply).await;
            panic!("a reply from a server that never sent one");
        },
        async { got.await.unwrap() },
    ));
    let dropped = Instant::now();
    drop(reply);
    closed
        .recv_timeout(PROMPT)
        .expect("the connection stays open");
    assert!(dropped.elapsed() < PROMPT);
}

#[test]
fn dropping_a_streaming_body_closes_the_connection() {
    let (port, _, closed) = server(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
          transfer-encoding: chunked\r\n\r\n\
          20\r\ndata: thinking\n\n",
    );
    let reply = rt::block_on(client().send(request(port))).unwrap();
    let mut body = reply.into_body();
    let chunk = next_chunk(&mut body).unwrap().unwrap();
    assert_eq!(&chunk[..], b"data: thinking\n\n");
    drop(body);
    closed
        .recv_timeout(PROMPT)
        .expect("the connection stays open");
}

#[test]
fn a_finished_reply_leaves_its_connection_to_the_next_request() {
    let http = client();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        for _ in 0..2 {
            read_request(&mut reader);
            (&stream)
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                .unwrap();
        }
    });
    for _ in 0..2 {
        let reply = rt::block_on(http.send(request(port))).unwrap();
        let mut body = reply.into_body();
        assert_eq!(&next_chunk(&mut body).unwrap().unwrap()[..], b"ok");
        assert!(next_chunk(&mut body).is_none());
    }
    server.join().unwrap();
}

/// Every stream at once, on one async worker and no blocking lane: none
/// waits for another to finish.
#[test]
fn concurrent_streams_never_queue_behind_each_other() {
    const STREAMS: usize = 64;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        // Every request arrives before any reply ends.
        let streams: Vec<_> = (0..STREAMS)
            .map(|_| {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                read_request(&mut reader);
                (&stream)
                    .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
                    .unwrap();
                stream
            })
            .collect();
        for mut stream in streams {
            stream.write_all(b"2\r\nok\r\n0\r\n\r\n").unwrap();
        }
    });
    let http = client();
    let replies = (0..STREAMS).map(|_| {
        let reply = http.send(request(port));
        async move {
            let body = reply.await.unwrap().into_body();
            let mut body = std::pin::pin!(body);
            let mut text = Vec::new();
            while let Some(chunk) =
                std::future::poll_fn(|cx| futures_core::Stream::poll_next(body.as_mut(), cx)).await
            {
                text.extend(chunk.unwrap());
            }
            text
        }
    });
    let all = rt::block_on(join_all(replies.collect()));
    assert!(all.iter().all(|text| text == b"ok"));
    server.join().unwrap();
}

/// Polls every future together until all are done.
async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut futures: Vec<_> = futures.into_iter().map(|f| Some(Box::pin(f))).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        for (future, output) in futures.iter_mut().zip(&mut outputs) {
            if let Some(f) = future
                && let std::task::Poll::Ready(out) = f.as_mut().poll(cx)
            {
                *output = Some(out);
                *future = None;
            }
        }
        if futures.iter().all(Option::is_none) {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    outputs.into_iter().map(Option::unwrap).collect()
}

#[test]
fn a_peer_closing_mid_request_body_fails_the_call() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // The head and a little of the body, then gone.
        stream.read_exact(&mut [0; 4096]).unwrap();
    });
    // Far more than socket buffers hold, so the client is still writing.
    let request = http::Request::post(format!("http://127.0.0.1:{port}/v1/responses"))
        .body(vec![b'x'; 64 << 20].into())
        .unwrap();
    let started = Instant::now();
    assert!(rt::block_on(client().send(request)).is_err());
    assert!(started.elapsed() < Duration::from_secs(10));
    server.join().unwrap();
}

#[test]
fn a_reply_cut_short_ends_in_an_error_not_an_end() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        read_request(&mut reader);
        (&stream)
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nhalf")
            .unwrap();
    });
    let reply = rt::block_on(client().send(request(port))).unwrap();
    server.join().unwrap();
    let mut body = reply.into_body();
    assert_eq!(&next_chunk(&mut body).unwrap().unwrap()[..], b"half");
    let error = next_chunk(&mut body).unwrap().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert!(next_chunk(&mut body).is_none());
}

#[test]
fn redirects_follow_by_policy_and_drop_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let headers = read_request(&mut reader);
            let reply: &[u8] = if headers.starts_with("get /moved") {
                b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\nhere"
            } else {
                b"HTTP/1.1 302 Found\r\nlocation: /moved\r\ncontent-length: 3\r\nconnection: close\r\n\r\ngo!"
            };
            (&stream).write_all(reply).unwrap();
            seen.push(headers);
        }
        seen
    });
    let rt: rt::Rt = Arc::new(Unblocked(rt::Threads::new(1)));
    let clients = Clients::new(rt);
    let mut texts = Vec::new();
    for policy in [RedirectPolicy::Follow, RedirectPolicy::Refuse] {
        let mut request = request(port);
        request
            .headers_mut()
            .insert(AUTHORIZATION, "secret".parse().unwrap());
        let reply = rt::block_on(clients.client(policy).send(request)).unwrap();
        let status = reply.status();
        let mut body = reply.into_body();
        let mut text = Vec::new();
        while let Some(chunk) = next_chunk(&mut body) {
            text.extend(chunk.unwrap());
        }
        texts.push((status.as_u16(), String::from_utf8(text).unwrap()));
    }
    assert_eq!(texts, [(200, "here".to_owned()), (302, "go!".to_owned())]);
    let seen = server.join().unwrap();
    assert!(seen[0].contains("authorization: secret"));
    assert!(
        seen[1].starts_with("get /moved") && !seen[1].contains("authorization"),
        "{}",
        seen[1]
    );
}

#[test]
fn credentials_never_cross_transport_scopes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        for credential in [Some("model-secret"), Some("mcp-secret"), None, None] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let headers = read_request(&mut reader);
            match credential {
                Some(value) => assert!(headers.contains(&format!("authorization: {value}\r\n"))),
                None => assert!(!headers.contains("authorization:")),
            }
            assert!(!headers.contains("cookie:"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nSet-Cookie: secret=private\r\nConnection: close\r\n\r\n").unwrap();
        }
    });
    let runtime = Arc::new(rt::Threads::new(1));
    let clients = Clients::new(runtime);
    for (policy, credential) in [
        (RedirectPolicy::Follow, Some("model-secret")),
        (RedirectPolicy::Refuse, Some("mcp-secret")),
        (RedirectPolicy::Follow, None),
        (RedirectPolicy::Refuse, None),
    ] {
        let mut request = request(port);
        if let Some(value) = credential {
            request
                .headers_mut()
                .insert(AUTHORIZATION, value.parse().unwrap());
        }
        let reply = rt::block_on(clients.client(policy).send(request)).unwrap();
        let mut body = reply.into_body();
        assert!(next_chunk(&mut body).is_none());
    }
    server.join().unwrap();
}

/// A body read in one call to its last chunk, then dropped, as a model
/// stream drops its reply at the last event: the connection still carries
/// the next request.
#[test]
fn a_reply_left_at_its_last_chunk_leaves_its_connection_to_the_next() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        for _ in 0..2 {
            read_request(&mut reader);
            (&stream)
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
                )
                .unwrap();
        }
    });
    let http = client();
    for _ in 0..2 {
        let reply = rt::block_on(http.send(request(port))).unwrap();
        let mut body = reply.into_body();
        assert_eq!(&next_chunk(&mut body).unwrap().unwrap()[..], b"hello");
    }
    server.join().unwrap();
}

#[test]
fn a_delayed_terminator_returns_a_dropped_replys_connection() {
    for (prefix, suffix) in [
        (&b"5\r\nhello\r\n"[..], &b"0\r\n\r\n"[..]),
        (&b"5\r\nhello\r\n0\r\n"[..], &b"\r\n"[..]),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (dropped, wait) = mpsc::channel();
        let (reused, done) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(IDLE)).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            read_request(&mut reader);
            (&stream)
                .write_all(
                    &[
                        &b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n"[..],
                        prefix,
                    ]
                    .concat(),
                )
                .unwrap();
            wait.recv().unwrap();
            (&stream).write_all(suffix).unwrap();
            read_request(&mut reader);
            (&stream)
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .unwrap();
            reused.send(()).unwrap();
        });
        let http = client();
        let reply = rt::block_on(http.send(request(port))).unwrap();
        let mut body = reply.into_body();
        assert_eq!(next_chunk(&mut body).unwrap().unwrap(), b"hello");
        drop(body);
        dropped.send(()).unwrap();
        // Wait for the background drain to return the connection, without
        // sending another request on a new socket in the meantime.
        let target = Target::try_from(request(port).uri()).unwrap();
        let started = Instant::now();
        loop {
            if let Some(conn) = http.0.pool.take(&target, http.0.rt.now()) {
                http.0.pool.put(target.clone(), conn, http.0.rt.now());
                break;
            }
            assert!(started.elapsed() < IDLE);
            thread::sleep(Duration::from_millis(1));
        }
        let _reply = rt::block_on(http.send(request(port))).unwrap();
        done.recv_timeout(IDLE).unwrap();
        server.join().unwrap();
    }
}

#[test]
fn a_reply_after_an_informational_one_in_the_same_read_is_not_waited_for() {
    let (port, _, _) = server(
        b"HTTP/1.1 103 Early Hints\r\nlink: </style.css>\r\n\r\n\
          HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
    );
    let started = Instant::now();
    let reply = rt::block_on(client().send(request(port))).unwrap();
    assert_eq!(reply.status(), 200);
    assert!(started.elapsed() < PROMPT, "{:?}", started.elapsed());
    let mut body = reply.into_body();
    assert_eq!(&next_chunk(&mut body).unwrap().unwrap()[..], b"ok");
}

#[test]
fn a_reply_gone_silent_between_chunks_times_out() {
    let (port, _, _) =
        server(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n");
    let reply = rt::block_on(client().send(request(port))).unwrap();
    let mut body = reply.into_body();
    assert_eq!(&next_chunk(&mut body).unwrap().unwrap()[..], b"hello");
    let started = Instant::now();
    let error = next_chunk(&mut body).unwrap().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() >= IDLE);
}

/// Provider keys follow a redirect within the host and stay behind when it
/// leaves; `Authorization` never follows.
#[test]
fn a_redirect_to_another_host_drops_the_provider_key() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let headers = read_request(&mut reader);
            let reply = if headers.starts_with("get /same") {
                format!(
                    "HTTP/1.1 302 Found\r\nlocation: http://localhost:{port}/other\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                )
            } else if headers.starts_with("get /other") {
                "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
            } else {
                "HTTP/1.1 303 See Other\r\nlocation: /same\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
            };
            (&stream).write_all(reply.as_bytes()).unwrap();
            seen.push(headers);
        }
        seen
    });
    // `localhost` is a name, looked up on the network lane.
    let http = Client::new(Arc::new(rt::Threads::new(1)), RedirectPolicy::Follow);
    let mut request = request(port);
    for (name, value) in [("x-api-key", "key"), ("authorization", "Bearer key")] {
        request.headers_mut().insert(name, value.parse().unwrap());
    }
    let reply = rt::block_on(http.send(request)).unwrap();
    assert_eq!(reply.status(), 200);
    let seen = server.join().unwrap();
    assert!(seen[0].contains("x-api-key: key") && seen[0].contains("authorization"));
    assert!(seen[1].contains("x-api-key: key"), "{}", seen[1]);
    assert!(!seen[1].contains("authorization"), "{}", seen[1]);
    assert!(!seen[2].contains("x-api-key"), "{}", seen[2]);
}

#[cfg(any(feature = "tls-native", feature = "tls-rustls"))]
mod tls_data {
    use std::net::TcpStream;

    use super::*;

    const CERT: &[u8] = include_bytes!("testdata/localhost.pem");
    const KEY: &[u8] = include_bytes!("testdata/localhost.key");

    trait Duplex: Read + Write {}
    impl<T: Read + Write> Duplex for T {}

    #[cfg(feature = "tls-native")]
    fn accept(tcp: TcpStream) -> Box<dyn Duplex> {
        let identity = native_tls::Identity::from_pkcs8(CERT, KEY).unwrap();
        Box::new(
            native_tls::TlsAcceptor::new(identity)
                .unwrap()
                .accept(tcp)
                .unwrap(),
        )
    }

    #[cfg(feature = "tls-rustls")]
    fn accept(tcp: TcpStream) -> Box<dyn Duplex> {
        use rustls::pki_types::pem::PemObject as _;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let chain = CertificateDer::pem_slice_iter(CERT)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(KEY).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
        let session = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        Box::new(rustls::StreamOwned::new(session, tcp))
    }

    fn trusting() -> Client {
        let mut tls = tls::Config::default();
        tls.trusted = Some(CERT.to_vec());
        let setup = Setup {
            proxies: Proxies::default(),
            tls,
        };
        Client::with(
            Arc::new(Unblocked(rt::Threads::new(1))),
            RedirectPolicy::Follow,
            Arc::new(setup),
        )
    }

    /// A peer reading slower than the client writes refuses writes for a
    /// while; every byte still reaches it, and the reply comes back.
    #[test]
    fn a_large_body_reaches_a_slow_reading_tls_peer_whole() {
        const BODY: usize = 8 << 20;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut tls = BufReader::new(accept(tcp));
            let mut line = String::new();
            while line != "\r\n" {
                line.clear();
                tls.read_line(&mut line).unwrap();
            }
            let mut received = 0;
            let mut buf = vec![0; 64 << 10];
            while received < BODY {
                let n = tls.read(&mut buf).unwrap();
                assert!(n > 0, "the client stopped after {received} bytes");
                received += n;
                thread::sleep(Duration::from_millis(1));
            }
            let text = received.to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{text}",
                text.len()
            );
            let mut tls = tls.into_inner();
            tls.write_all(reply.as_bytes()).unwrap();
            tls.flush().unwrap();
            // Until the client has read the reply.
            let _ = tls.read(&mut buf);
        });
        let request = http::Request::post(format!("https://localhost:{port}/v1/responses"))
            .body(vec![b'x'; BODY].into())
            .unwrap();
        // `localhost` would be looked up on the blocking lane: dial the
        // literal the certificate also names.
        let request = {
            let (mut parts, body) = request.into_parts();
            parts.uri = format!("https://127.0.0.1:{port}/v1/responses")
                .parse()
                .unwrap();
            http::Request::from_parts(parts, body)
        };
        let reply = rt::block_on(trusting().send(request)).unwrap();
        assert_eq!(reply.status(), 200);
        let mut body = reply.into_body();
        assert_eq!(
            next_chunk(&mut body).unwrap().unwrap(),
            BODY.to_string().into_bytes()
        );
        drop(body);
        server.join().unwrap();
    }
}

#[test]
fn request_lines_and_headers_grow_without_a_transport_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let headers = read_request(&mut reader);
        assert!(headers.lines().next().unwrap().len() > 16 << 10);
        assert!(
            headers
                .lines()
                .any(|line| line.starts_with("x-large: ") && line.len() > 64 << 10)
        );
        (&stream)
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .unwrap();
    });
    let request = http::Request::post(format!("http://127.0.0.1:{port}/{}", "x".repeat(32 << 10)))
        .header("x-large", "y".repeat(128 << 10))
        .body(Payload::default())
        .unwrap();
    assert_eq!(rt::block_on(client().send(request)).unwrap().status(), 200);
    server.join().unwrap();
}

fn compressed_events(coding: &str) -> (Vec<u8>, Vec<u8>) {
    use futures_lite::io::AsyncWriteExt as _;
    macro_rules! encode {
        ($encoder:ty) => {{
            rt::block_on(async {
                let mut encoder = <$encoder>::new(Vec::new());
                encoder.write_all(b"data: first\n\n").await.unwrap();
                encoder.flush().await.unwrap();
                let first = encoder.get_ref().clone();
                encoder.write_all(b"data: last\n\n").await.unwrap();
                encoder.close().await.unwrap();
                (first, encoder.into_inner())
            })
        }};
    }
    match coding {
        "gzip" => encode!(async_compression::futures::write::GzipEncoder<Vec<u8>>),
        "br" => encode!(async_compression::futures::write::BrotliEncoder<Vec<u8>>),
        "zstd" => {
            let mut compressor = mzstd::Compressor::new(mzstd::EncoderConfig::default()).unwrap();
            let mut output = Vec::new();
            let mut writer = compressor.writer(&mut output, mzstd::StreamConfig::default());
            writer.write_all(b"data: first\n\n").unwrap();
            writer.flush().unwrap();
            let first = (*writer.get_ref()).clone();
            writer.write_all(b"data: last\n\n").unwrap();
            writer.finish().unwrap();
            (first, output)
        }
        _ => unreachable!(),
    }
}

#[test]
fn compressed_events_arrive_before_the_next_event_or_footer() {
    for coding in ["gzip", "br", "zstd"] {
        let (first, all) = compressed_events(coding);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (received, wait) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            assert!(read_request(&mut reader).contains("accept-encoding: gzip, br, zstd"));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-encoding: {coding}\r\ncontent-length: {}\r\n\r\n",
                all.len()
            )
            .unwrap();
            stream.write_all(&first).unwrap();
            wait.recv_timeout(IDLE)
                .expect("decoder held an event until EOF");
            stream.write_all(&all[first.len()..]).unwrap();
        });
        let reply = rt::block_on(client().send(request(port))).unwrap();
        assert!(!reply.headers().contains_key(CONTENT_ENCODING));
        assert!(!reply.headers().contains_key(CONTENT_LENGTH));
        let mut body = reply.into_body();
        assert_eq!(
            next_chunk(&mut body).unwrap().unwrap(),
            b"data: first\n\n",
            "{coding}"
        );
        received.send(()).unwrap();
        let mut rest = Vec::new();
        while let Some(chunk) = next_chunk(&mut body) {
            rest.extend(chunk.unwrap());
        }
        assert_eq!(rest, b"data: last\n\n", "{coding}");
        server.join().unwrap();
    }
}

#[test]
fn truncated_compression_is_an_error() {
    for coding in ["gzip", "br", "zstd"] {
        let (first, _) = compressed_events(coding);
        let response = http::Response::builder()
            .header(CONTENT_ENCODING, coding)
            .body(crate::body([first]))
            .unwrap();
        let mut body = compression::decode(response).unwrap().into_body();
        let mut error = false;
        while let Some(chunk) = next_chunk(&mut body) {
            error |= chunk.is_err();
        }
        assert!(error, "{coding} ended successfully without its footer");
    }
}

#[test]
fn compressed_uploads_keep_every_payload_piece_and_set_the_wire_length() {
    for coding in [crate::Compression::Gzip, crate::Compression::Zstd] {
        let payload: Payload = (0..4096).map(|i| Arc::<[u8]>::from(format!("message {i}: the conversation includes code and tool output with repeated keys\n").into_bytes())).collect::<Vec<_>>().into();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let original = payload.len();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            let mut encoding = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-encoding:") {
                    encoding = value.trim().to_owned();
                }
            }
            assert_eq!(
                encoding,
                match coding {
                    crate::Compression::Gzip => "gzip",
                    crate::Compression::Zstd => "zstd",
                }
            );
            assert!(length < original);
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\ncontent-encoding: {encoding}\r\ncontent-length: {length}\r\n\r\n").unwrap();
            stream.write_all(&bytes).unwrap();
        });
        let mut request = request(port);
        *request.body_mut() = payload.clone();
        request.extensions_mut().insert(coding);
        let mut body = rt::block_on(client().send(request)).unwrap().into_body();
        let mut received = Vec::new();
        while let Some(chunk) = next_chunk(&mut body) {
            received.extend(chunk.unwrap());
        }
        assert_eq!(received, payload.to_vec());
        server.join().unwrap();
    }
}

#[test]
fn a_dropped_boundary_without_a_terminator_is_closed_by_the_drain_deadline() {
    let (port, _, closed) =
        server(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n");
    let http = client();
    let mut body = rt::block_on(http.send(request(port))).unwrap().into_body();
    assert_eq!(next_chunk(&mut body).unwrap().unwrap(), b"hello");
    let started = Instant::now();
    drop(body);
    closed
        .recv_timeout(IDLE + PROMPT)
        .expect("terminator drain outlived its deadline");
    assert!(started.elapsed() >= IDLE);
}

#[test]
fn an_upload_may_outlive_the_idle_deadline_while_the_peer_keeps_reading() {
    const BYTES: usize = 16 << 20;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        // Windows can buffer this entire request. Send the head now so
        // its deadline does not measure the peer's deliberately slow reads.
        (&stream)
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .unwrap();
        let mut bytes = vec![0; 64 << 10];
        for _ in 0..BYTES / bytes.len() {
            reader.read_exact(&mut bytes).unwrap();
            thread::sleep(Duration::from_millis(20));
        }
    });
    let mut request = request(port);
    *request.body_mut() = vec![b'x'; BYTES].into();
    let http = client();
    let started = Instant::now();
    assert_eq!(rt::block_on(http.send(request)).unwrap().status(), 200);
    server.join().unwrap();
    assert!(started.elapsed() > IDLE);
}
