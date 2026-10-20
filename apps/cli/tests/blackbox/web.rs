//! `a web` against an echo-agent daemon, driven over HTTP as a browser and
//! Datastar would: the link's token for a cookie, the gate, a chat's move
//! to a host of its own, the pages, and a tab's event stream patching in
//! the replies to what the tab sends.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::common::{Env, thread_names, wait_for};
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
        Self::spawn(env.cmd(&["web", "--no-open"]))
    }

    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd
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

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn redeem(&self) -> Response {
        self.fetch("GET", &format!("/?token={}", self.token), &[], "")
    }

    fn sign_in(&mut self) {
        let response = self.redeem();
        let set = response.header("set-cookie").expect("no cookie");
        set.split(';').next().unwrap().clone_into(&mut self.cookie);
    }

    /// Sends a request with `headers`, the main Host unless one is given,
    /// and a JSON `body`; returns the open connection.
    fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> TcpStream {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\n");
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            let _ = write!(request, "Host: {}\r\n", self.host());
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
        let mut bytes = Vec::new();
        self.send(method, path, headers, body)
            .read_to_end(&mut bytes)
            .unwrap();
        // A compressed body is not text; its head is.
        Response(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A same-origin request of the signed-in browser, on `host`.
    fn ours_on(&self, host: &str, method: &str, path: &str, body: &str) -> Response {
        let origin = format!("http://{host}");
        let headers = [
            ("Host", host),
            ("Cookie", self.cookie.as_str()),
            ("Origin", &origin),
        ];
        self.fetch(method, path, &headers, body)
    }

    fn ours(&self, method: &str, path: &str, body: &str) -> Response {
        self.ours_on(&self.host(), method, path, body)
    }

    /// A tab's call, as Datastar makes it: the page's signals as the body.
    fn call(&self, path: &str, tab: &str, draft: &str) -> Response {
        let signals = serde_json::json!({ "tab": tab, "draft": draft }).to_string();
        self.ours("POST", path, &signals)
    }

    /// Opens a live transcript for tab `tab` of `session`.
    fn events(&self, session: &str, tab: &str) -> Events {
        let headers = [
            ("Cookie", self.cookie.as_str()),
            ("Accept", "text/event-stream"),
            ("Datastar-Request", "true"),
        ];
        let path = format!("/s/{session}/events?tab={tab}");
        Events {
            stream: self.send("GET", &path, &headers, ""),
            raw: String::new(),
            patches: Vec::new(),
        }
    }

    /// Starts a session from the list page; its id.
    fn new_session(&self) -> String {
        let created = self.ours("POST", "/new", "");
        assert_eq!(created.status(), 303, "{}", created.0);
        let path = created.header("location").unwrap();
        path.strip_prefix("/s/").unwrap().to_owned()
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

    fn body(&self) -> &str {
        self.0.split_once("\r\n\r\n").unwrap().1
    }
}

/// The text between `start` and the next `end` after it.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    Some(&text[from..from + text[from..].find(end)?])
}

/// One `datastar-patch-elements` event.
#[derive(Debug)]
struct Patch {
    selector: Option<String>,
    mode: Option<String>,
    html: String,
}

/// A tab's event stream, decoded as Datastar decodes it.
struct Events {
    stream: TcpStream,
    raw: String,
    patches: Vec<Patch>,
}

impl Events {
    /// Reads until the patches so far satisfy `done`.
    fn until(&mut self, what: &str, done: impl Fn(&[Patch]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut buf = [0; 16 * 1024];
        while !done(&self.patches) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            let n = self
                .stream
                .read(&mut buf)
                .unwrap_or_else(|e| panic!("{e} waiting for {what}: {:?}", self.patches));
            assert_ne!(n, 0, "the stream ended before {what}: {}", self.raw);
            self.raw.push_str(&String::from_utf8_lossy(&buf[..n]));
            self.decode();
        }
    }

    fn decode(&mut self) {
        // The head first, once.
        if self.patches.is_empty()
            && let Some((head, _)) = self.raw.split_once("\r\n\r\n")
            && head.starts_with("HTTP/")
        {
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            assert!(head.contains("content-type: text/event-stream"), "{head}");
            let skip = head.len() + 4;
            self.raw.drain(..skip);
        }
        while let Some(at) = self.raw.find("\n\n") {
            let event: String = self.raw.drain(..at + 2).collect();
            let mut patch = Patch {
                selector: None,
                mode: None,
                html: String::new(),
            };
            let mut lines = Vec::new();
            for line in event.lines() {
                let Some(data) = line.strip_prefix("data: ") else {
                    assert!(
                        line == "event: datastar-patch-elements" || line.is_empty(),
                        "{line:?}"
                    );
                    continue;
                };
                let (key, value) = data.split_once(' ').unwrap_or((data, ""));
                match key {
                    "selector" => patch.selector = Some(value.to_owned()),
                    "mode" => patch.mode = Some(value.to_owned()),
                    "elements" => lines.push(value),
                    _ => panic!("{line:?}"),
                }
            }
            patch.html = lines.join("\n");
            self.patches.push(patch);
        }
    }
}

/// Everything the patches carried.
fn shown(patches: &[Patch]) -> String {
    patches.iter().map(|p| p.html.as_str()).collect()
}

/// The transcript's end as the latest patch that carried it shows it.
fn end(patches: &[Patch]) -> &str {
    patches
        .iter()
        .rev()
        .find_map(|p| p.html.find("<div id=\"end\"").map(|at| &p.html[at..]))
        .unwrap_or_default()
}

/// Clients the daemon counts on each session.
fn clients(env: &Env) -> Vec<u32> {
    let mut conn = env.connect();
    conn.send(&ClientMsg::List);
    let mut list = SessionList::default();
    while !list.fold(&conn.recv()) {}
    list.sessions.iter().map(|s| s.clients).collect()
}

const TAB: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn the_link_signs_in_once_and_foreign_requests_are_refused() {
    let env = Env::new("web-gate");
    let mut web = Web::start(&env);
    assert_eq!(web.fetch("GET", "/", &[], "").status(), 403);
    assert_eq!(web.fetch("GET", "/?token=0123", &[], "").status(), 403);
    let link = format!("/?token={}", web.token);
    let other_port = format!("t1.localhost:{}", web.port + 1);
    for host in [
        "evil.test",
        "localhost.evil.test",
        &format!("a.b.localhost:{}", web.port),
        &format!("evil.test:{}", web.port),
        &other_port,
    ] {
        let rebound = web.fetch("GET", &link, &[("Host", host)], "");
        assert_eq!(rebound.status(), 421, "{host} spent the link");
    }

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
    for origin in [
        "http://evil.test",
        "null",
        "http://127.0.0.1:1",
        "http://t1.localhost.evil.test",
    ] {
        let response = web.fetch("POST", "/new", &[cookie, ("Origin", origin)], "");
        assert_eq!(response.status(), 403, "{origin}");
    }
    let list = web.ours("GET", "/", "");
    assert_eq!(list.status(), 200);
    assert!(list.0.contains("No sessions yet."), "{}", list.0);
    let path = between(&list.0, "src=\"/assets/", "\"").expect("the browser script");
    let script = web.ours("GET", &format!("/assets/{path}"), "");
    assert_eq!(script.status(), 200, "{path}");
    assert_eq!(script.header("content-type"), Some("text/javascript"));
    assert_eq!(script.header("content-encoding"), Some("deflate"));
}

/// Browsers open six connections to a host, and each chat tab holds one
/// for its event stream: a chat opened on the main host moves to a host of
/// its own, signed in by a token good once.
#[test]
fn a_chat_moves_to_a_host_of_its_own() {
    let env = Env::new("web-hosts");
    let mut web = Web::start(&env);
    web.sign_in();
    let session = web.new_session();
    let chat = format!("/s/{session}");

    let mut hosts = Vec::new();
    for _ in 0..2 {
        let moved = web.ours("GET", &chat, "");
        assert_eq!(moved.status(), 303, "{}", moved.0);
        let location = moved.header("location").unwrap();
        let rest = location.strip_prefix("http://").unwrap();
        let (host, path) = rest.split_once('/').unwrap();
        assert!(
            host.ends_with(&format!(".localhost:{}", web.port)),
            "{location}"
        );
        let (path, token) = path.split_once("?token=").unwrap();
        assert_eq!(format!("/{path}"), chat);
        hosts.push((host.to_owned(), token.to_owned()));
    }
    assert_ne!(hosts[0].0, hosts[1].0, "two tabs on one host");

    // The first visit signs the new host in and is the page itself.
    let (host, token) = &hosts[0];
    let handed = format!("{chat}?token={token}");
    let page = web.fetch("GET", &handed, &[("Host", host)], "");
    assert_eq!(page.status(), 200, "{}", page.0);
    let set = page.header("set-cookie").unwrap();
    assert!(set.contains("; SameSite=Strict"), "{set}");
    assert!(page.body().contains("Connecting…"), "{}", page.0);
    assert!(page.body().contains("/events?tab="), "{}", page.0);
    assert_eq!(
        web.fetch("GET", &handed, &[("Host", host)], "").status(),
        403,
        "the token worked twice"
    );
    // A reload, signed in: the page again, without the spent token.
    let reload = web.ours_on(host, "GET", &handed, "");
    assert_eq!(reload.status(), 303);
    assert_eq!(reload.header("location"), Some(chat.as_str()));
    let again = web.ours_on(host, "GET", &chat, "");
    assert_eq!(again.status(), 200, "{}", again.0);
}

#[test]
fn a_tab_streams_replies_and_answers_asks_until_it_closes() {
    let env = Env::new("web-chat");
    env.ext("policy", "ask = [\"bash\"]");
    let mut web = Web::start(&env);
    web.sign_in();
    let session = web.new_session();

    let mut events = web.events(&session, TAB);
    events.until("the attach", |p| shown(p).contains("env "));
    assert!(
        events.patches[0].html.starts_with("<div id=\"transcript\""),
        "{:?}",
        events.patches[0]
    );
    wait_for("the tab to attach", || clients(&env) == [1]);

    let sent = web.call("/api/send", TAB, "hello web");
    assert_eq!(sent.status(), 200, "{}", sent.0);
    // Datastar takes it as a patch of the page's signals.
    assert_eq!(sent.header("content-type"), Some("application/json"));
    assert_eq!(sent.body(), r#"{"draft":""}"#);
    events.until("the line and its echo", |p| {
        shown(p).matches(">hello web<").count() == 2
    });

    web.call("/api/send", TAB, "$ echo web-ok");
    events.until("the ask", |p| shown(p).contains("Run this?"));
    let html = shown(&events.patches);
    let ask = &html[..html.find("Run this?").unwrap()];
    let call = between(&ask[ask.rfind("id=\"i").unwrap()..], "id=\"i", "\"").unwrap();
    let approve = format!("/api/approve?call={call}&ok=1");
    assert_eq!(web.call(&approve, TAB, "").status(), 204);
    events.until("the output", |p| {
        p.iter().any(|p| {
            let html = &p.html;
            html.contains(">web-ok<") && html.contains("exit ")
        })
    });

    // A running call offers Stop, which interrupts the turn.
    web.call("/api/send", TAB, "$ sleep 30");
    let asked = |p: &Patch| p.html.contains("Run this?") && p.html.contains("sleep 30");
    events.until("the second ask", |p| p.iter().any(asked));
    let html = &events.patches.iter().find(|p| asked(p)).unwrap().html;
    let call = between(html, "id=\"i", "\"").unwrap();
    let approve = format!("/api/approve?call={call}&ok=1");
    assert_eq!(web.call(&approve, TAB, "").status(), 204);
    events.until("the stop control", |p| end(p).contains(">Stop<"));
    assert_eq!(web.call("/api/interrupt", TAB, "").status(), 204);
    events.until("the call to stop", |p| {
        let shown = shown(p);
        !end(p).contains(">Stop<")
            // Stop shows before the process starts, so the interrupt may
            // also land before the spawn: then nothing ran, and the call
            // ends aborted.
            && (shown.contains(">interrupted<")
                || shown.contains("killed by")
                || shown.contains("[aborted]")
                || shown.contains(">aborted<"))
    });

    // Closing the tab detaches its client; stopping `a web` leaves the
    // daemon and its session running.
    drop(events);
    wait_for("the tab to detach", || clients(&env) == [0]);
    assert_eq!(web.call("/api/send", TAB, "anyone there?").status(), 404);
    let pid = env.pid();
    drop(web);
    assert!(pid.is_some());
    assert_eq!(clients(&env), [0]);
    assert_eq!(env.pid(), pid);
}

/// Each change goes out alone: a new item is added before the end, and
/// nothing shown is sent again, however long the transcript grows.
#[test]
fn a_long_transcript_sends_each_item_once() {
    let env = Env::new("web-long");
    let mut web = Web::start(&env);
    web.sign_in();
    let session = web.new_session();
    let mut events = web.events(&session, TAB);
    events.until("the attach", |p| shown(p).contains("env "));
    let shown_whole = events.patches.len();
    for n in 0..40 {
        let text = format!("note {n}.");
        web.call("/api/send", TAB, &text);
        // The note, then its echo.
        let line = format!(">{text}<");
        events.until(&text, |p| shown(p).matches(&line).count() >= 2);
    }
    for patch in &events.patches[shown_whole..] {
        let added =
            patch.selector.as_deref() == Some("#end") && patch.mode.as_deref() == Some("before");
        let end = patch.selector.is_none() && patch.html.starts_with("<div id=\"end\"");
        assert!(added || end, "{patch:?}");
    }
    let html = shown(&events.patches);
    for n in 0..40 {
        let count = html.matches(&format!(">note {n}.<")).count();
        assert_eq!(count, 2, "note {n} shown {count} times");
    }
}

/// A tab's event stream is a task on `a web`'s reactor, not a thread: a
/// session open in twenty tabs, each holding its stream, costs the
/// process no thread more than one tab does. Not yet: the stream is on
/// the reactor, but each tab's link is a `client::Driver` connection, and
/// the driver reads every connection on a thread of its own (D1 in the
/// apps audit; 9 threads with one tab, 28 with twenty).
#[test]
#[ignore = "a tab's link is a Driver reader thread until the client reads on the reactor"]
fn an_idle_tab_costs_no_thread() {
    let env = Env::new("web-threads");
    let mut web = Web::start(&env);
    web.sign_in();
    let session = web.new_session();
    let tab = |n: usize| format!("{n:032x}");
    let mut first = web.events(&session, &tab(0));
    first.until("the attach", |p| shown(p).contains("env "));
    let one = thread_names(web.child.id());
    let mut tabs: Vec<_> = (1..20).map(|n| web.events(&session, &tab(n))).collect();
    for events in &mut tabs {
        events.until("the attach", |p| shown(p).contains("env "));
    }
    let twenty = thread_names(web.child.id());
    eprintln!(
        "a web: {} threads with 1 tab, {} with 20",
        one.len(),
        twenty.len()
    );
    assert_eq!(twenty.len(), one.len(), "{one:?} -> {twenty:?}");
}

/// A daemon upgrade does not end a tab: it attaches to the new daemon from
/// where it was, and goes on.
#[test]
fn a_tab_rides_out_a_daemon_upgrade() {
    let env = Env::new("web-drain");
    let old = env.older_build();
    let mut web = Web::spawn(env.cmd_as(&old, &["web", "--no-open"]));
    web.sign_in();
    let session = web.new_session();
    let mut events = web.events(&session, TAB);
    events.until("the attach", |p| shown(p).contains("env "));
    web.call("/api/send", TAB, "before");
    events.until("the echo", |p| shown(p).matches(">before<").count() == 2);
    let first = env.pid().unwrap();

    // The newer build drains the older one's daemon and starts its own.
    env.run(&["ls"]);
    let second = env.pid().unwrap();
    assert_ne!(second, first);
    wait_for("the tab to attach again", || clients(&env) == [1]);
    let shown_before = events.patches.len();
    assert_eq!(web.call("/api/send", TAB, "after").status(), 200);
    events.until("the echo after", |p| {
        shown(p).matches(">after<").count() >= 2
    });
    let since = shown(&events.patches[shown_before..]);
    assert!(!since.contains("reload to reconnect"), "{since}");
    // Shown whole again once the new daemon synced, then patched.
    assert!(since.contains(">before<"), "{since}");
    assert_eq!(env.daemons(), [second]);
}
