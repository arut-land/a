//! `a web`: the web UI, a client process like the CLI. The server renders
//! every page and patch from Rust (hypertext's typed markup) and hosts them
//! on its own small HTTP server over the `rt` runtime; in the browser,
//! Datastar applies the patches each tab's event stream sends. Each live
//! transcript in a browser tab is its own daemon client; closing `a web`
//! stops only the UI.

mod app;
mod http;
mod page;
/// The page state, exposed for cross-renderer integration tests.
#[doc(hidden)]
pub use page::Page;
mod patch;
mod style;
/// Items as markup: public for the test that every renderer shows an item
/// the same way.
#[doc(hidden)]
pub mod view;

use std::io::{self, Write};
use std::net::TcpListener;
use std::sync::Arc;

use client::Daemons;
use rt::Rt;

/// Serves the UI on a random port of 127.0.0.1 until the process is
/// stopped. The printed link carries a one-time token; `open` also opens it
/// in the default browser.
pub fn run(daemons: Daemons, open: bool) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let gate = http::Gate::new(port);
    let link = gate.link();
    let rt: Rt = Arc::new(rt::Threads::default());
    let app = Arc::new(app::App::new(daemons, Arc::clone(&rt), gate));

    // Whoever reads the link may stop reading (`a web | head -1`): the
    // server keeps going without its stdout.
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "a web: {link}").and_then(|()| {
        writeln!(
            out,
            "[the link works once; stop with Ctrl-C, sessions keep running]"
        )
    });
    drop(out);
    if open {
        match sys::open_browser(&link) {
            Ok(opener) => rt::reap(&*rt, opener),
            Err(e) => eprintln!("[could not open a browser: {e}; open the link above]"),
        }
    }
    // Each connection is a task of its own; this thread only waits for the
    // next, through the reactor.
    let listener = rt::Async::socket(&rt.reactor(), listener)?;
    rt::block_on(async {
        loop {
            let (stream, _) =
                accept(async || listener.read_with(TcpListener::accept).await).await?;
            let task = http::serve(Arc::clone(&rt), Arc::clone(&app), stream);
            rt.spawn(Box::pin(task));
        }
    })
}

async fn accept<T>(mut next: impl AsyncFnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match next().await {
            // These failures leave the listener available for the next connection.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                ) => {}
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt::Runtime;
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    #[test]
    fn exhausted_descriptors_do_not_spin_on_a_pending_connection() {
        // Descriptor limits belong to this subprocess, never the test runner.
        if std::env::var_os("A_WEB_ACCEPT_EXHAUSTION").is_none() {
            if !std::path::Path::new("/dev/null").exists() {
                return;
            }
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", "ulimit -n 64; exec \"$@\"", "sh"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::exhausted_descriptors_do_not_spin_on_a_pending_connection",
                    "--nocapture",
                ])
                .env("A_WEB_ACCEPT_EXHAUSTION", "1")
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "descriptor exhaustion child: {status}");
                    return;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("accept did not return with exhausted descriptors");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let rt = rt::Threads::new(1);
        let listener =
            rt::Async::socket(&rt.reactor(), TcpListener::bind(("127.0.0.1", 0)).unwrap()).unwrap();
        let _pending = TcpStream::connect(listener.get_ref().local_addr().unwrap()).unwrap();
        let mut files = Vec::new();
        let exhausted = loop {
            match std::fs::File::open("/dev/null") {
                Ok(file) => files.push(file),
                Err(error) => break error,
            }
        };
        let error = rt::block_on(accept(async || {
            listener.read_with(TcpListener::accept).await
        }))
        .unwrap_err();
        assert_eq!(error.raw_os_error(), exhausted.raw_os_error());
    }

    #[test]
    fn failed_accept_is_returned_without_retrying_the_listener() {
        let rt = rt::Threads::new(1);
        let listener =
            rt::Async::socket(&rt.reactor(), TcpListener::bind(("127.0.0.1", 0)).unwrap()).unwrap();
        for error in [
            // EMFILE and WSAEMFILE, interpreted by each native host.
            io::Error::from_raw_os_error(24),
            io::Error::from_raw_os_error(10024),
            io::ErrorKind::OutOfMemory.into(),
            io::ErrorKind::PermissionDenied.into(),
        ] {
            let kind = error.kind();
            let code = error.raw_os_error();
            let mut failure = Some(error);
            let returned = rt::block_on(accept(async || {
                listener
                    .read_with(|_| {
                        Err::<(), _>(
                            failure
                                .take()
                                .expect("a persistent accept failure was retried"),
                        )
                    })
                    .await
            }))
            .unwrap_err();
            assert_eq!(returned.kind(), kind);
            assert_eq!(returned.raw_os_error(), code);
        }
    }

    #[test]
    fn a_transient_accept_failure_preserves_the_next_connection() {
        let rt = rt::Threads::new(1);
        let listener =
            rt::Async::socket(&rt.reactor(), TcpListener::bind(("127.0.0.1", 0)).unwrap()).unwrap();
        let peer = TcpStream::connect(listener.get_ref().local_addr().unwrap()).unwrap();
        let mut errors = [
            io::ErrorKind::Interrupted,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
        ]
        .into_iter();
        let (stream, _) = rt::block_on(accept(async || {
            listener
                .read_with(|listener| match errors.next() {
                    Some(kind) => Err(kind.into()),
                    None => listener.accept(),
                })
                .await
        }))
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), peer.local_addr().unwrap());
    }
}
