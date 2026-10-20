//! `a web`: the web UI, a client process like the CLI. The server renders
//! every page and patch from Rust (hypertext's typed markup) and hosts them
//! on its own small HTTP server over the `rt` runtime; in the browser,
//! Datastar applies the patches each tab's event stream sends. Each live
//! transcript in a browser tab is its own daemon client; closing `a web`
//! stops only the UI.

mod app;
mod http;
mod page;
mod patch;
mod style;
/// Items as markup: public for the test that every renderer shows an item
/// the same way.
#[doc(hidden)]
pub mod view;

use std::io::{self, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use client::Daemon;
use rt::Rt;

/// Serves the UI on a random port of 127.0.0.1 until the process is
/// stopped. The printed link carries a one-time token; `open` also opens it
/// in the default browser.
pub fn run(daemon: Daemon, open: bool) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let gate = http::Gate::new(port);
    let link = gate.link();
    let rt: Rt = Arc::new(rt::Threads::default());
    let app = Arc::new(app::App::new(daemon, Arc::clone(&rt), gate));

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
    if open && let Err(e) = open_browser(&link) {
        eprintln!("[could not open a browser: {e}; open the link above]");
    }
    // Each connection is a task of its own; this thread only waits for the
    // next, through the reactor.
    let listener = rt::Async::socket(&rt.reactor(), listener)?;
    rt::block_on(async {
        loop {
            let Ok((stream, _)) = listener.read_with(TcpListener::accept).await else {
                continue;
            };
            let task = http::serve(Arc::clone(&rt), Arc::clone(&app), stream);
            rt.spawn(Box::pin(task));
        }
    })
}

fn open_browser(link: &str) -> io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = Command::new(program)
        .arg(link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reaped aside: some openers stay until the browser they started exits.
    thread::spawn(move || child.wait());
    Ok(())
}
