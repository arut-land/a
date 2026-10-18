//! `a web`: the web UI, a client process like the CLI. topcoat renders the
//! pages; our own small HTTP server hosts them on the `rt` runtime, so no
//! tokio runtime runs. Each live transcript in a browser tab is its own
//! daemon client; closing `a web` stops only the UI.

mod app;
mod http;
mod style;
mod view;

use std::io;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use client::Endpoint;
use rt::Rt;
use sys::Staged;

/// The daemon `a web` is a client of, and how it is reached.
#[derive(Clone)]
pub struct Daemon {
    pub endpoint: Endpoint,
    pub staged: Staged,
}

/// Serves the UI on a random port of 127.0.0.1 until the process is
/// stopped. The printed link carries a one-time token; `open` also opens it
/// in the default browser.
pub fn run(daemon: Daemon, open: bool) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let gate = Arc::new(http::Gate::new(port));
    let link = gate.link();
    let rt: Rt = Arc::new(rt::Threads::default());
    let router = app::router(daemon, Arc::clone(&rt));

    println!("a web: {link}");
    println!("[the link works once; stop with Ctrl-C, sessions keep running]");
    if open && let Err(e) = open_browser(&link) {
        eprintln!("[could not open a browser: {e}; open the link above]");
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let task = http::serve(Arc::clone(&rt), router.clone(), Arc::clone(&gate), stream);
        rt.spawn(Box::pin(task));
    }
    Ok(())
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
