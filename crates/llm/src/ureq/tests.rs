use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::pin::Pin;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;

/// Well past a socket shutdown's wake-up, well short of any timeout.
const PROMPT: Duration = Duration::from_millis(200);

/// [`rt::Threads`], telling `done` each time a blocking job returns.
struct Watched {
    inner: rt::Threads,
    done: mpsc::Sender<()>,
}

impl rt::Runtime for Watched {
    fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
        self.inner.spawn(task);
    }

    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        let done = self.done.clone();
        self.inner.spawn_blocking(Box::new(move || {
            work();
            let _ = done.send(());
        }));
    }

    fn sleep(&self, duration: Duration) -> rt::BoxFuture<'static, ()> {
        self.inner.sleep(duration)
    }

    fn reactor(&self) -> rt::Reactor {
        self.inner.reactor()
    }
}

/// A client, and the channel its blocking jobs report returning on.
fn client() -> (Ureq, mpsc::Receiver<()>) {
    let (done, returned) = mpsc::channel();
    let rt = Watched {
        inner: rt::Threads::new(1),
        done,
    };
    (Ureq::new(Arc::new(rt)), returned)
}

fn request(port: u16) -> http::Request<Payload> {
    http::Request::post(format!("http://127.0.0.1:{port}/v1/responses"))
        .body(b"{}".to_vec().into())
        .unwrap()
}

/// Reads one request's head and body off `reader`.
fn read_request(reader: &mut BufReader<TcpStream>) {
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
    }
    reader.read_exact(&mut vec![0; length]).unwrap();
}

/// A server for one connection: it reads a request, says so on `got`,
/// writes `reply`, then reads until the client closes the connection and
/// says so on `closed`.
fn server(reply: &'static [u8]) -> (u16, mpsc::Receiver<()>, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (got, got_rx) = mpsc::channel();
    let (closed, closed_rx) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        read_request(&mut reader);
        (&stream).write_all(reply).unwrap();
        got.send(()).unwrap();
        // EOF or a reset: either way the client is gone.
        let _ = reader.read_to_end(&mut Vec::new());
        closed.send(()).unwrap();
    });
    (port, got_rx, closed_rx)
}

fn next_chunk(body: &mut crate::http::Body) -> Option<io::Result<Vec<u8>>> {
    rt::block_on(std::future::poll_fn(|cx| {
        futures_core::Stream::poll_next(Pin::as_mut(body), cx)
    }))
}

#[test]
fn dropping_a_request_waiting_for_its_head_closes_the_connection() {
    let (http, returned) = client();
    let (port, got, closed) = server(b"");
    let reply = http.send(request(port));
    got.recv().unwrap();
    let dropped = Instant::now();
    drop(reply);
    closed
        .recv_timeout(PROMPT)
        .expect("the connection stays open");
    returned
        .recv_timeout(PROMPT)
        .expect("the blocking thread is held");
    assert!(dropped.elapsed() < PROMPT);
}

#[test]
fn dropping_a_streaming_body_closes_the_connection() {
    let (http, returned) = client();
    let (port, got, closed) = server(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
          transfer-encoding: chunked\r\n\r\n\
          10\r\ndata: thinking\n\n\r\n",
    );
    let reply = rt::block_on(http.send(request(port))).unwrap();
    got.recv().unwrap();
    let mut body = reply.into_body();
    let chunk = next_chunk(&mut body).unwrap().unwrap();
    assert_eq!(&chunk[..], b"data: thinking\n\n");
    drop(body);
    closed
        .recv_timeout(PROMPT)
        .expect("the connection stays open");
    returned
        .recv_timeout(PROMPT)
        .expect("the blocking thread is held");
}

#[test]
fn a_finished_reply_leaves_its_connection_to_the_next_request() {
    let (http, _returned) = client();
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
