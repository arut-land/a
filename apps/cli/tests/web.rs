//! `a web` against an echo-agent daemon, driven over HTTP as a browser and
//! its topcoat runtime would: the link's token for a cookie, the gate, the
//! pages, and a live transcript that streams the replies to what the tab
//! sends.
#![cfg(all(unix, feature = "web"))]

mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::{Env, wait_for};
use protocol::ClientMsg;
use protocol::view::SessionList;

/// A running `a web`, stopped on drop.
struct Web {
    child: Child,
    port: u16,
    token: String,
    /// The session cookie, once signed in.
    cookie: String,
}

impl Web {
    fn start(env: &Env) -> Self {
        let mut child = env
            .cmd(&["web", "--no-open"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut printed = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut printed)
            .unwrap();
        let link = printed
            .trim()
            .strip_prefix("a web: http://127.0.0.1:")
            .unwrap_or_else(|| panic!("no link in {printed:?}"));
        let (port, token) = link.split_once("/?token=").unwrap();
        Self {
            port: port.parse().unwrap(),
            token: token.to_owned(),
            child,
            cookie: String::new(),
        }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn redeem(&self) -> Response {
        self.fetch("GET", &format!("/?token={}", self.token), &[], "")
    }

    fn sign_in(&mut self) {
        let response = self.redeem();
        let set = response.header("set-cookie").expect("no cookie");
        set.split(';').next().unwrap().clone_into(&mut self.cookie);
    }

    /// Sends a request with `headers`, the right Host unless one is given,
    /// and a JSON `body`; returns the open connection.
    fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\n");
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            let _ = write!(request, "Host: 127.0.0.1:{}\r\n", self.port);
        }
        for (name, value) in headers {
            let _ = write!(request, "{name}: {value}\r\n");
        }
        let _ = write!(
            request,
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        stream
    }

    fn fetch(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Response {
        let mut text = String::new();
        self.send(method, path, headers, body)
            .read_to_string(&mut text)
            .unwrap();
        Response(text)
    }

    /// A same-origin request of the signed-in browser.
    fn ours(&self, method: &str, path: &str, body: &str) -> Response {
        let origin = self.origin();
        let headers = [("Cookie", self.cookie.as_str()), ("Origin", &origin)];
        self.fetch(method, path, &headers, body)
    }

    /// Calls procedure `path` with `args`, returning its JSON result.
    fn call(&self, path: &str, args: &str) -> String {
        self.ours("POST", path, args).body()
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Response(String);

impl Response {
    fn status(&self) -> u16 {
        self.0[9..12].parse().unwrap()
    }

    fn header(&self, name: &str) -> Option<&str> {
        let head = self.0.split("\r\n\r\n").next().unwrap();
        head.lines().find_map(|line| {
            let (k, v) = line.split_once(": ")?;
            k.eq_ignore_ascii_case(name).then_some(v)
        })
    }

    /// The body, unchunked.
    fn body(&self) -> String {
        let (_, mut rest) = self.0.split_once("\r\n\r\n").unwrap();
        if self.header("transfer-encoding") != Some("chunked") {
            return rest.to_owned();
        }
        let mut body = String::new();
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let size = usize::from_str_radix(size, 16).unwrap();
            body.push_str(&tail[..size]);
            rest = &tail[size + 2..];
        }
        body
    }
}

/// The text between `start` and the next `end` after it.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    Some(&text[from..from + text[from..].find(end)?])
}

/// A live transcript: the streamed response of the transcript shard.
struct Transcript {
    stream: TcpStream,
    seen: String,
}

impl Transcript {
    /// Reads until the latest emission satisfies `done`.
    fn until(&mut self, what: &str, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut buf = [0; 16 * 1024];
        // Each emission is one NDJSON frame.
        while !done(self.seen.rsplit("\n{").next().unwrap()) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            let n = self.stream.read(&mut buf).unwrap();
            assert_ne!(n, 0, "the stream ended before {what}: {}", self.seen);
            self.seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }
}

/// Clients the daemon counts on each session.
fn clients(env: &Env) -> Vec<u32> {
    let mut conn = env.connect();
    conn.send(&ClientMsg::List);
    let mut list = SessionList::default();
    while !list.fold(&conn.recv()) {}
    list.sessions.iter().map(|s| s.clients).collect()
}

#[test]
fn the_link_signs_in_once_and_foreign_requests_are_refused() {
    let env = Env::new("web-gate");
    let mut web = Web::start(&env);
    assert_eq!(web.fetch("GET", "/", &[], "").status(), 403);
    assert_eq!(web.fetch("GET", "/?token=0123", &[], "").status(), 403);
    let link = format!("/?token={}", web.token);
    let rebound = web.fetch("GET", &link, &[("Host", "evil.test")], "");
    assert_eq!(rebound.status(), 421, "a foreign Host spent the link");

    let redeemed = web.redeem();
    assert_eq!(redeemed.status(), 303, "{}", redeemed.0);
    assert_eq!(redeemed.header("location"), Some("/"));
    let set = redeemed.header("set-cookie").unwrap();
    assert!(
        set.contains("; HttpOnly") && set.contains("; SameSite=Strict"),
        "{set}"
    );
    assert_eq!(web.redeem().status(), 403, "the link worked twice");

    set.split(';').next().unwrap().clone_into(&mut web.cookie);
    let cookie = ("Cookie", web.cookie.as_str());
    for origin in ["http://evil.test", "null", "http://127.0.0.1:1"] {
        let response = web.fetch("POST", "/new", &[cookie, ("Origin", origin)], "");
        assert_eq!(response.status(), 403, "{origin}");
    }
    let list = web.ours("GET", "/", "");
    assert_eq!(list.status(), 200);
    assert!(list.0.contains("No sessions yet."), "{}", list.0);
    let script = web.ours("GET", "/assets/topcoat.js", "");
    assert_eq!(script.header("content-type"), Some("text/javascript"));
}

#[test]
fn a_tab_streams_replies_and_answers_asks_until_it_closes() {
    let mut env = Env::new("web-chat");
    env.vars.push(("A_ASK", "bash"));
    let mut web = Web::start(&env);
    web.sign_in();
    let created = web.ours("POST", "/new", "");
    assert_eq!(created.status(), 303, "{}", created.0);
    let page_path = created.header("location").unwrap().to_owned();
    let session = page_path.strip_prefix("/s/").unwrap();

    // The page renders at once; its transcript is a shard that streams when
    // asked again with `live` set, as the runtime does once it runs.
    let page = web.ours("GET", &page_path, "");
    assert_eq!(page.status(), 200);
    assert!(page.0.contains("Connecting…"), "{}", page.0);
    let identity = between(&page.0, "shard::start(\"/api/transcript\", \"", "\"").unwrap();
    let tab = "0123456789abcdef0123456789abcdef";
    let headers = [
        ("Cookie", web.cookie.as_str()),
        ("Accept", "application/x-ndjson"),
        ("X-Topcoat-Identity", identity),
    ];
    let rerun = format!(r#"{{"args":["{tab}","{session}",true],"signals":{{}}}}"#);
    let mut transcript = Transcript {
        stream: web.send("POST", "/api/transcript", &headers, &rerun),
        seen: String::new(),
    };
    transcript.until("the attach", |s| s.contains("environment"));
    assert!(
        transcript.seen.starts_with("HTTP/1.1 200"),
        "{}",
        transcript.seen
    );
    wait_for("the tab to attach", || clients(&env) == [1]);

    let send = |text: &str| web.call("/api/send", &serde_json::to_string(&(tab, text)).unwrap());
    assert_eq!(send("hello web"), "true");
    transcript.until("the line and its echo", |s| {
        s.matches(">hello web<").count() == 2
    });

    send("$ echo web-ok");
    transcript.until("the ask", |s| s.contains("Run this?"));
    let call = between(&transcript.seen, "id=\\\"call-", "\\\"").unwrap();
    let answer = format!(r#"["{tab}",{{"t":"u64","bits":64,"v":"{call}"}},true]"#);
    assert_eq!(web.call("/api/approve", &answer), "true");
    transcript.until("the output", |s| {
        s.contains("approved") && s.contains("web-ok\\n") && s.contains("exit ")
    });

    // Closing the tab detaches its client; stopping `a web` leaves the
    // daemon and its session running.
    drop(transcript);
    wait_for("the tab to detach", || clients(&env) == [0]);
    assert_eq!(send("anyone there?"), "false");
    let pid = env.pid();
    drop(web);
    assert!(pid.is_some());
    assert_eq!(clients(&env), [0]);
    assert_eq!(env.pid(), pid);
}
