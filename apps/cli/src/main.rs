mod args;
mod exts;

use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::thread;

use client::{Attach, Driver, Effect, End, Endpoint, Env, Event, Link, SessionClient};
use protocol::view::{Change, Item, Kind, Part, Presented, SessionView, Shown, recap_line};
use protocol::{CallId, ClientMsg, DaemonMsg, SessionSummary, Target, Verdict};
use sys::{Home, Staged};

use crate::args::{Args, Cmd, Daemon};

fn main() -> ExitCode {
    // Help, the version, completion requests and usage errors (status 2)
    // end the process here.
    let args = Args::parse();
    let result = Ctx::new(args.host).and_then(|ctx| match args.cmd {
        None => session(ctx, Target::New),
        Some(Cmd::Resume { id: None, any }) => session(ctx, Target::Latest { any_workspace: any }),
        Some(Cmd::Resume { id: Some(id), .. } | Cmd::Attach { id }) => {
            session(ctx, Target::Prefix(id))
        }
        Some(Cmd::Ls) => ls(&ctx),
        Some(Cmd::New) => new(&ctx),
        Some(Cmd::Bridge { token }) => {
            client::bridge(&ctx.home, token.as_deref().unwrap_or(&ctx.staged.id))
        }
        Some(Cmd::Web { no_open }) => web(ctx, !no_open),
        Some(Cmd::Daemon { action }) => daemon_cmd(&ctx, action),
        Some(Cmd::Serve {
            listen_fd,
            lock_fd,
            reset_breaker,
        }) => daemon(&ctx, listen_fd, lock_fd, reset_breaker),
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("a: {e}");
            ExitCode::FAILURE
        }
    }
}

/// This build, staged, and the daemon it talks to.
#[derive(Clone)]
struct Ctx {
    home: Home,
    staged: Staged,
    endpoint: Endpoint,
}

impl Ctx {
    /// Every entry point stages itself first, so the daemon it may spawn
    /// runs the newest build.
    fn new(host: Option<String>) -> io::Result<Self> {
        let home = Home::from_env()?;
        let staged = home.runtime().stage(&std::env::current_exe()?)?;
        let endpoint = host.map_or_else(|| Endpoint::Local(home.clone()), Endpoint::Ssh);
        Ok(Self {
            home,
            staged,
            endpoint,
        })
    }

    /// The home, for commands that only make sense on this machine.
    fn local(&self) -> io::Result<&Home> {
        match &self.endpoint {
            Endpoint::Local(home) => Ok(home),
            Endpoint::Ssh(host) => Err(io::Error::other(format!(
                "this command acts on this machine; run it on {host} instead"
            ))),
        }
    }

    /// Connects to the daemon. A wedged local daemon is diagnosed, and
    /// `a daemon restart` suggested; a session at a terminal offers the
    /// restart itself.
    fn connect(&self) -> io::Result<client::Connection> {
        self.connect_with(&|_| Ok(Vec::new()))
    }

    /// [`Ctx::connect`], sending what `first` makes right behind the hello.
    fn connect_with(
        &self,
        first: &dyn Fn(&client::Connection) -> io::Result<Vec<ClientMsg>>,
    ) -> io::Result<client::Connection> {
        let on_drain = || eprintln!("[waiting for the previous daemon to finish its work]");
        client::connect_with(&self.endpoint, &self.staged, None, &on_drain, first).map_err(|e| {
            match &self.endpoint {
                Endpoint::Local(home) if e.kind() == io::ErrorKind::TimedOut => {
                    report(&client::diagnose(home));
                    io::Error::new(e.kind(), RESTART)
                }
                _ => e,
            }
        })
    }
}

const RESTART: &str = "run `a daemon restart` to replace it";

fn daemon_cmd(ctx: &Ctx, action: Daemon) -> io::Result<()> {
    let home = ctx.local()?;
    match action {
        Daemon::Status => {
            let d = client::diagnose(home);
            match (d.pid, client::status(home)) {
                (None, _) | (_, Ok(None)) => println!("not running"),
                (_, Ok(Some(s))) => println!(
                    "running (pid {}, {}), {} sessions, {} other clients",
                    s.pid,
                    s.build,
                    s.sessions,
                    s.clients.saturating_sub(1)
                ),
                (Some(pid), Err(e)) => println!("running (pid {pid}) but not responding: {e}"),
            }
            println!("log: {}", d.log.display());
            if let Some(note) = d.crash_note() {
                println!("{note}");
            }
        }
        Daemon::Stop => {
            let stopped = client::stop(home)?;
            println!("{}", if stopped { "stopped" } else { "not running" });
        }
        Daemon::Restart => {
            client::restart(home, &ctx.staged, None)?;
            println!("restarted");
        }
    }
    Ok(())
}

/// The full-screen client at a terminal, the line REPL anywhere else (or
/// with `A_TUI=plain`).
fn session(ctx: Ctx, target: Target) -> io::Result<()> {
    match tui::viewport() {
        Some(viewport) => {
            let daemon = tui::Daemon {
                endpoint: ctx.endpoint,
                staged: ctx.staged,
            };
            tui::run(&daemon, target, viewport)
        }
        None => repl(&ctx, target),
    }
}

/// Connects and makes `attach` right behind the hello.
fn attach(ctx: &Ctx, attach: Attach) -> io::Result<client::Connection> {
    ctx.connect_with(&move |conn| Ok(vec![conn.attach(&attach)?]))
}

/// What wakes the line client.
enum Input {
    /// A line typed; `None` once stdin closed.
    Line(Option<String>),
    Client(Event),
}

/// The line-mode client: the shared session client, its updates printed
/// as lines and its intents typed. When the daemon drains for an upgrade,
/// it reconnects (starting the new build) and replays what it missed.
fn repl(ctx: &Ctx, target: Target) -> io::Result<()> {
    let (tx, rx) = client::inbox();
    let events = tx.clone();
    let driver = Driver::new(ctx.endpoint.clone(), ctx.staged.clone(), move |_, event| {
        let _ = events.send(Input::Client(event));
    });
    let mut client = SessionClient::new(Env::Origin);
    let mut repl = Repl {
        screen: Screen::default(),
        input: Some(tx),
        live: false,
    };
    let mut effects = client.open(target);
    loop {
        for effect in effects {
            if let Some(effect) = driver.apply(effect)
                && let Some(exit) = repl.show(&client, effect)
            {
                return exit;
            }
        }
        let Some(input) = rx.recv() else {
            return Ok(());
        };
        effects = match input {
            Input::Client(event) => client.step(event),
            Input::Line(None) => client.detach(),
            Input::Line(Some(line)) => match client.link() {
                Link::Wedged(d) => match line.trim() {
                    "r" => client.restart(),
                    "l" => {
                        for line in d.log_tail() {
                            eprintln!("  | {line}");
                        }
                        prompt();
                        Vec::new()
                    }
                    _ => return Err(io::Error::new(io::ErrorKind::TimedOut, NOT_RESPONDING)),
                },
                _ => match client::parse(&line, client.turn().asking) {
                    None => Vec::new(),
                    Some(client::Cmd::Help) => {
                        for (usage, help) in client::help(client.commands()) {
                            println!("  {usage:<16} {help}");
                        }
                        Vec::new()
                    }
                    Some(cmd) => client.run(cmd),
                },
            },
        };
    }
}

const NOT_RESPONDING: &str = "daemon not responding";

struct Repl {
    screen: Screen,
    /// Taken by the thread reading stdin, once attached.
    input: Option<client::Post<Input>>,
    /// Attached once at least.
    live: bool,
}

impl Repl {
    /// Shows what the client's `effect` tells; `Some` ends the client.
    fn show(&mut self, client: &SessionClient, effect: Effect) -> Option<io::Result<()>> {
        match effect {
            Effect::View(update) => {
                // Nothing to attach to: no such session.
                if let (None, Change::Error { message }) = (client.view().session, &update.change) {
                    return Some(Err(io::Error::other(message.clone())));
                }
                self.screen.show(&update.change, client.view());
            }
            Effect::Notice(text) => eprintln!("[{text}]"),
            Effect::Sessions => {
                for s in client.sessions() {
                    println!("  {}", listed(s));
                }
            }
            Effect::Link => return self.link(client.link()),
            _ => {}
        }
        None
    }

    fn link(&mut self, link: &Link) -> Option<io::Result<()>> {
        match link {
            Link::Live { .. } => {
                self.live = true;
                self.read_stdin();
            }
            Link::Draining if !self.live => {
                eprintln!("[waiting for the previous daemon to finish its work]");
            }
            Link::Reconnecting { .. } => eprintln!("[daemon upgraded; reconnecting]"),
            Link::Wedged(d) => {
                report(d);
                if !io::stdin().is_terminal() {
                    return Some(Err(io::Error::new(io::ErrorKind::TimedOut, RESTART)));
                }
                prompt();
                self.read_stdin();
            }
            Link::Ended(End::Detached) => {
                println!("[detached; the session keeps running. `a resume` to reattach]");
                return Some(Ok(()));
            }
            Link::Ended(End::Lost(why)) if self.live => {
                return Some(Err(io::Error::other(format!("daemon disconnected: {why}"))));
            }
            Link::Ended(End::Lost(why)) => return Some(Err(io::Error::other(why.clone()))),
            Link::Connecting | Link::Draining => {}
        }
        None
    }

    /// Reads typed lines from here on, which a first attach is waited for:
    /// piped input then goes to the session it attached.
    fn read_stdin(&mut self) {
        let Some(tx) = self.input.take() else {
            return;
        };
        thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(Input::Line(Some(line))).is_err() {
                    return;
                }
            }
            let _ = tx.send(Input::Line(None));
        });
    }
}

fn prompt() {
    eprint!("  [r] restart daemon  [l] show log  [q] quit: ");
    let _ = io::stderr().flush();
}

/// What a client can tell of a daemon that does not answer.
fn report(d: &client::Diagnosis) {
    eprintln!(
        "! {} is not responding after {}s",
        d.who(),
        client::HELLO_TIMEOUT.as_secs()
    );
    eprintln!("  log: {}", d.log.display());
    if let Some(note) = d.crash_note() {
        eprintln!("  {note}");
    }
}

/// Prints the shared view's updates as transcript lines, each item as
/// [`Item::present`] has it. A model reply streams in as deltas; on replay
/// there are none, so the committed text is printed instead. What waits
/// for an approval is asked once the replay is in, after its recap, and as
/// it begins to wait from then on.
#[derive(Default)]
struct Screen {
    /// The call whose streamed reply is on the current line.
    streaming: Option<CallId>,
    /// The replay of the latest attach is in.
    synced: bool,
}

impl Screen {
    fn show(&mut self, change: &Change, view: &SessionView) {
        match change {
            Change::Attached {
                session, workspace, ..
            } => {
                self.synced = false;
                println!("[session {} · {}]", session.short(), workspace.display());
            }
            Change::Delta { call, text } => {
                if self.streaming != Some(*call) {
                    self.streaming = Some(*call);
                    print!("a    ");
                }
                print!("{text}");
                let _ = io::stdout().flush();
            }
            Change::Added { item, .. } => {
                // A retry streams its reply anew, on a line of its own.
                if let Item::Hook {
                    call: Some(call),
                    decision: Verdict::Retry { .. },
                    ..
                } = item
                    && self.streaming.take_if(|s| s == call).is_some()
                {
                    println!();
                }
                print_all(&added(item));
            }
            // Sent in place of a long replay: shown as the replay would be.
            Change::Snapshot => {
                for item in &view.items {
                    print_all(&added(item));
                    if item.settled() {
                        print_all(&outcome(&item.present()));
                    }
                }
            }
            Change::Updated { index, part } => self.updated(&view.items[*index], *part),
            Change::Synced => {
                self.synced = true;
                if let Some(recap) = &view.recap {
                    println!("[{}]", recap_line(recap));
                }
                for call in view.ledger.inbox() {
                    if let Some(item) = view.items.iter().find(|i| i.key() == call.0) {
                        println!("{}", ask(&item.present()));
                    }
                }
            }
            Change::Mismatch { supported } => eprintln!("[daemon speaks protocol {supported}]"),
            Change::Error { message } => eprintln!("[error: {message}]"),
            Change::Draining => {}
        }
    }

    fn updated(&mut self, item: &Item, part: Part) {
        let p = item.present();
        match (part, p.kind) {
            (Part::Outcome, Kind::Reply) => {
                // A streamed reply is on screen already; end its line.
                let streamed = self.streaming.take_if(|s| s.0 == item.key()).is_some();
                if streamed {
                    println!();
                } else if let Shown::Markdown(text) = p.body
                    && !text.is_empty()
                {
                    println!("a    {text}");
                }
                if let Some(usage) = p.usage {
                    println!("     · {usage}");
                }
                if !p.status.is_empty() {
                    println!("  └ {}", p.status);
                }
            }
            (Part::Outcome, Kind::Tool | Kind::Command | Kind::Child) => print_all(&outcome(&p)),
            (Part::Phase, _) if self.synced && p.buttons != protocol::view::Buttons::None => {
                println!("{}", ask(&p));
            }
            (Part::Answer { approved }, _) => {
                println!("  · {}", if approved { "approved" } else { "not approved" });
            }
            _ => {}
        }
    }
}

fn print_all(lines: &[String]) {
    for line in lines {
        println!("{line}");
    }
}

/// The question a call waiting for approval puts.
fn ask(p: &Presented<'_>) -> String {
    format!("? approve {}  [y/n]", p.title)
}

/// The lines an item starts with, as it is added.
fn added(item: &Item) -> Vec<String> {
    let p = item.present();
    match p.kind {
        Kind::User => vec![format!("you  {}", p.body.text())],
        Kind::Assistant => vec![format!("a    {}", p.body.text())],
        Kind::Command => vec![format!("cmd  {}", p.title)],
        Kind::Tool => vec![format!("tool {}", p.title)],
        Kind::Hook | Kind::Environment => vec![format!("  · {}", p.title)],
        Kind::Child => vec![format!("{} {}", p.title, p.body.text())],
        // A reply is printed when done: as its text streams, or whole. A
        // side request's reply is its extension's, not the conversation's.
        Kind::Reply | Kind::Side => Vec::new(),
    }
}

/// The lines a settled call or command ends with: its body behind a
/// gutter, then its status.
fn outcome(p: &Presented<'_>) -> Vec<String> {
    let body = p.body.text();
    let mut lines: Vec<String> = body
        .lines()
        .filter(|_| !matches!(p.body, Shown::None))
        .map(|line| format!("  │ {line}"))
        .collect();
    lines.push(format!("  └ {}", p.status));
    lines
}

fn ls(ctx: &Ctx) -> io::Result<()> {
    for s in ctx.connect()?.sessions()? {
        println!("{}", listed(&s));
    }
    Ok(())
}

/// A session as `a ls` lists it.
fn listed(s: &SessionSummary) -> String {
    let kept = s
        .read_only
        .map_or_else(String::new, |why| format!("  ({why})"));
    format!(
        "{}  {:<9}  {:<10}  {:>4} entries  {}{kept}",
        s.id.short(),
        s.activity.name(),
        s.residency.name(),
        s.entries,
        s.workspace.display()
    )
}

fn new(ctx: &Ctx) -> io::Result<()> {
    let mut conn = attach(ctx, Attach::new(Target::New, Env::Origin))?;
    let session = conn.reader.until(|msg| match msg {
        DaemonMsg::Attached { session, .. } => Some(session),
        _ => None,
    })?;
    println!("{}", session.short());
    conn.writer.send(&ClientMsg::Detach)
}

#[cfg(feature = "web")]
fn web(ctx: Ctx, open: bool) -> io::Result<()> {
    let daemon = web::Daemon {
        endpoint: ctx.endpoint,
        staged: ctx.staged,
    };
    web::run(daemon, open)
}

#[cfg(not(feature = "web"))]
fn web(_ctx: Ctx, _open: bool) -> io::Result<()> {
    Err(io::Error::other("this build of a has no web UI"))
}

/// The daemon. Started by a client (`listen_fd`), it leaves the client's
/// session and logs to `logs/daemon.log`; by hand, it stays at the
/// terminal.
fn daemon(
    ctx: &Ctx,
    listen_fd: Option<i32>,
    lock_fd: Option<i32>,
    reset_breaker: bool,
) -> io::Result<()> {
    let home = &ctx.home;
    sys::crash::install(home.crash_dir(), ctx.staged.id.clone());
    let listener = if let Some(fd) = listen_fd {
        sys::detach_session()?;
        sys::log::redirect_stderr(&home.daemon_log())?;
        sys::Listener::adopt(fd.into())?
    } else {
        home.bind()?
    };
    let start = engine::Start {
        lock: lock_fd.map(|fd| sys::adopt_lock(fd.into())).transpose()?,
        reset_breaker,
        logs: listen_fd.is_some(),
    };
    engine::serve(
        &listener,
        home,
        &engine::Config {
            extensions: exts::registry,
            ..engine::Config::load(home)
        },
        &ctx.staged.id,
        start,
    )
}

/// Every renderer shows an item the same way: whatever one says of an
/// item (its title, its status, its body), the others say too, over the
/// shared view vectors. The layout is each renderer's; the words are not.
#[cfg(all(test, unix, feature = "web"))]
mod parity {
    use protocol::view::{Kind, SessionView, Shown};
    use protocol::{DaemonMsg, Decode};

    /// Text as a renderer that cannot wrap, colour or indent would have it.
    fn bare(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The text of markup: tags dropped, entities read back.
    fn text_of_html(html: &str) -> String {
        let mut text = String::new();
        let mut tag = false;
        for c in html.chars() {
            match c {
                '<' => tag = true,
                '>' => tag = false,
                c if !tag => text.push(c),
                _ => {}
            }
        }
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&")
    }

    #[test]
    fn every_renderer_shows_an_item_the_same_way() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/protocol/tests/vectors/view.json"
        );
        let vectors: Vec<serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut items = 0;
        for vector in vectors {
            let mut view = SessionView::default();
            for m in vector["messages"].as_array().unwrap() {
                view.fold(&DaemonMsg::from_json(m.to_string().as_bytes()).unwrap());
            }
            for item in &view.items {
                let p = item.present();
                if p.kind == Kind::Side {
                    continue;
                }
                items += 1;
                let look = tui::view::Look {
                    verbose: true,
                    ..tui::view::Look::default()
                };
                let tui = tui::view::texts(&tui::view::rows(item, 200, look)).join("\n");
                let web = text_of_html(&web::view::html(web::view::ItemRow { item, view: &view }));
                let mut repl = super::added(item);
                if item.settled() {
                    repl.extend(super::outcome(&p));
                }
                let repl = repl.join("\n");
                let said = [
                    bare(&p.title),
                    bare(&p.status),
                    match &p.body {
                        Shown::None => String::new(),
                        body => bare(body.text().lines().next().unwrap_or_default()),
                    },
                ];
                for (name, shown) in [("tui", &tui), ("web", &web), ("repl", &repl)] {
                    let shown = bare(shown);
                    for word in &said {
                        assert!(
                            shown.contains(word.as_str()),
                            "{name} leaves {word:?} out of {item:?}:\n{shown}"
                        );
                    }
                }
            }
        }
        assert!(items > 20, "{items} items checked");
    }
}
