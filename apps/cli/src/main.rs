/// [`out`]s a formatted line.
macro_rules! sayln {
    () => {
        $crate::out(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::out(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// [`out`]s formatted text.
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::out(format_args!($($arg)*))
    };
}

mod args;
mod exts;
mod once;

use std::fmt;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::thread;

use client::{Driver, End, Endpoint, Env, Event, Link, Renderer, SessionClient, Show};
use protocol::view::{
    Change, Item, Kind, Part, Presented, SessionView, Shown, answer_line, mismatch_line, recap_line,
};
use protocol::{CallId, Target, Verdict};
use sys::Home;

use crate::args::{Args, Cmd, Daemon};

fn main() -> ExitCode {
    // Help, the version, completion requests and usage errors (status 2)
    // end the process here.
    let args = Args::parse();
    let (prompt, mode) = (args.prompt, args.mode);
    let result = daemon_of(args.host).and_then(|daemon| {
        let done = match args.cmd {
            None => return session(&daemon, Target::New, prompt, mode),
            Some(Cmd::Resume { id: None, any }) => {
                return session(&daemon, Target::Latest { any_workspace: any }, prompt, mode);
            }
            Some(Cmd::Resume { id: Some(id), .. } | Cmd::Attach { id }) => {
                return session(&daemon, Target::Prefix(id), prompt, mode);
            }
            Some(_) if prompt.is_some() => {
                eprintln!("a: -p runs a turn: it takes no command but `resume` or `attach`");
                return Ok(ExitCode::from(2));
            }
            Some(Cmd::Ls) => ls(&daemon),
            Some(Cmd::New) => new(&daemon),
            Some(Cmd::Bridge { token }) => client::bridge(
                daemon.home()?,
                token.as_deref().unwrap_or(&daemon.staged.id),
            ),
            Some(Cmd::Web { no_open }) => web(daemon, !no_open),
            Some(Cmd::Daemon { action }) => daemon_cmd(&daemon, action),
            Some(Cmd::Serve {
                listen_fd,
                lock_fd,
                reset_breaker,
            }) => serve(&daemon, listen_fd, lock_fd, reset_breaker),
        };
        done.map(|()| ExitCode::SUCCESS)
    });
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("a: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The status `a` ends with when stdout's reader has gone: 128 + SIGPIPE,
/// as a program SIGPIPE ended would.
const STDOUT_GONE: i32 = 141;

/// Writes `args` to stdout, at once. A reader that has gone (`a ls | head
/// -1`) ends `a` there, quietly, as SIGPIPE ends a C program; a session it
/// was attached to goes on in the daemon.
fn out(args: fmt::Arguments<'_>) {
    let mut stdout = io::stdout().lock();
    if let Err(e) = stdout.write_fmt(args).and_then(|()| stdout.flush())
        && e.kind() == io::ErrorKind::BrokenPipe
    {
        std::process::exit(STDOUT_GONE);
    }
}

/// The daemon to talk to, as this build: every entry point stages itself
/// first, so the daemon it may spawn runs the newest build.
fn daemon_of(host: Option<String>) -> io::Result<client::Daemon> {
    let home = Home::from_env()?;
    let staged = home.runtime().stage(&std::env::current_exe()?)?;
    Ok(match host {
        Some(host) => client::Daemon::ssh(host, staged),
        None => client::Daemon::local(home, staged),
    })
}

/// Connects to the daemon. A wedged local daemon is diagnosed, and
/// `a daemon restart` suggested; a session at a terminal offers the
/// restart itself.
fn connect(daemon: &client::Daemon) -> io::Result<client::Connection> {
    let on_drain = || eprintln!("[waiting for the previous daemon to finish its work]");
    daemon.connect(&on_drain).map_err(|e| wedged(daemon, e))
}

/// `e` as reported: a local daemon that does not answer is diagnosed.
fn wedged(daemon: &client::Daemon, e: client::Error) -> io::Error {
    match (&daemon.endpoint, &e) {
        (Endpoint::Local(home), client::Error::NotResponding) => {
            report(&client::diagnose(home));
            io::Error::new(io::ErrorKind::TimedOut, RESTART)
        }
        _ => e.into(),
    }
}

const RESTART: &str = "run `a daemon restart` to replace it";

fn daemon_cmd(daemon: &client::Daemon, action: Daemon) -> io::Result<()> {
    let home = daemon.home()?;
    match action {
        Daemon::Status => {
            let d = client::diagnose(home);
            match (d.pid, client::status(home)) {
                (None, _) | (_, Ok(None)) => sayln!("not running"),
                (_, Ok(Some(s))) => sayln!(
                    "running (pid {}, {}), {} sessions, {} other clients",
                    s.pid,
                    s.build,
                    s.sessions,
                    s.clients.saturating_sub(1)
                ),
                (Some(pid), Err(e)) => sayln!("running (pid {pid}) but not responding: {e}"),
            }
            sayln!("log: {}", d.log.display());
            if let Some(note) = d.crash_note() {
                sayln!("{note}");
            }
        }
        Daemon::Stop => {
            let stopped = client::stop(home)?;
            sayln!("{}", if stopped { "stopped" } else { "not running" });
        }
        Daemon::Restart => {
            daemon.restart()?;
            sayln!("restarted");
        }
    }
    Ok(())
}

/// One turn and out ([`once`]) on a prompt, or on what is piped to stdin
/// (unless `A_TUI=plain` asks for the line REPL); otherwise the full-screen
/// client at a terminal, the line REPL anywhere else, which leave `mode` to
/// `/mode`.
fn session(
    daemon: &client::Daemon,
    target: Target,
    prompt: Option<String>,
    mode: Option<String>,
) -> io::Result<ExitCode> {
    if prompt.is_some() || (!io::stdin().is_terminal() && !tui::plain()) {
        return once::run(daemon, target, prompt, mode);
    }
    match tui::viewport() {
        Some(viewport) => tui::run(daemon, target, viewport),
        None => repl(daemon, target),
    }
    .map(|()| ExitCode::SUCCESS)
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
fn repl(daemon: &client::Daemon, target: Target) -> io::Result<()> {
    let (tx, rx) = client::inbox();
    let events = tx.clone();
    let driver = Driver::new(daemon.clone(), move |_, event| {
        let _ = events.send(Input::Client(event));
    });
    let mut repl = Repl {
        client: SessionClient::new(Env::Origin),
        screen: Screen::default(),
        input: Some(tx),
        live: false,
        exit: None,
    };
    let mut effects = repl.client.open(target);
    loop {
        driver.pump(&mut repl, effects);
        if let Some(exit) = repl.exit.take() {
            return exit;
        }
        let Some(input) = rx.recv() else {
            return Ok(());
        };
        effects = match input {
            Input::Client(event) => repl.client.step(event),
            Input::Line(None) => repl.client.detach(),
            Input::Line(Some(line)) => repl.line(&line)?,
        };
    }
}

const NOT_RESPONDING: &str = "daemon not responding";

struct Repl {
    client: SessionClient,
    screen: Screen,
    /// Taken by the thread reading stdin, once attached.
    input: Option<client::Post<Input>>,
    /// Attached once at least.
    live: bool,
    /// Set once something shown ends the client.
    exit: Option<io::Result<()>>,
}

impl Repl {
    /// What a typed line asks for. While the daemon does not answer, the
    /// line answers the prompt instead.
    fn line(&mut self, line: &str) -> io::Result<Vec<client::Effect>> {
        if let Link::Wedged(d) = self.client.link() {
            return match line.trim() {
                "r" => Ok(self.client.restart()),
                "l" => {
                    for line in d.log_tail() {
                        eprintln!("  | {line}");
                    }
                    prompt();
                    Ok(Vec::new())
                }
                _ => Err(io::Error::new(io::ErrorKind::TimedOut, NOT_RESPONDING)),
            };
        }
        Ok(match client::parse(line, self.client.turn().asking) {
            None => Vec::new(),
            Some(client::Cmd::Help) => {
                for (usage, help) in client::help(self.client.commands()) {
                    sayln!("  {usage:<16} {help}");
                }
                Vec::new()
            }
            Some(cmd) => self.client.run(cmd),
        })
    }

    fn link(&mut self) -> Option<io::Result<()>> {
        match self.client.link().clone() {
            Link::Replaying { .. } | Link::Live { .. } => {
                self.live = true;
                self.read_stdin();
            }
            Link::Draining if !self.live => {
                eprintln!("[waiting for the previous daemon to finish its work]");
            }
            Link::Reconnecting { .. } => eprintln!("[daemon upgraded; reconnecting]"),
            Link::Wedged(d) => {
                report(&d);
                if !io::stdin().is_terminal() {
                    return Some(Err(io::Error::new(io::ErrorKind::TimedOut, RESTART)));
                }
                prompt();
                self.read_stdin();
            }
            Link::Ended(End::Detached) => {
                sayln!("[detached; the session keeps running. `a resume` to reattach]");
                return Some(Ok(()));
            }
            Link::Ended(End::Lost(why)) if self.live => {
                return Some(Err(io::Error::other(format!("daemon disconnected: {why}"))));
            }
            Link::Ended(End::Lost(why)) => return Some(Err(why.into())),
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

/// Shows what the client tells; what ends the client is kept for the loop.
impl Renderer for Repl {
    fn show(&mut self, shown: Vec<Show>) -> Vec<client::Effect> {
        for show in shown {
            if self.exit.is_some() {
                break;
            }
            match show {
                Show::View(update) => {
                    // Nothing to attach to: no such session.
                    if let (None, Change::Error { message }) =
                        (self.client.view().session, &update.change)
                    {
                        self.exit = Some(Err(io::Error::other(message.clone())));
                        continue;
                    }
                    self.screen.show(&update.change, self.client.view());
                }
                Show::Notice(notice) => eprintln!("[{notice}]"),
                Show::Sessions => {
                    for s in self.client.sessions() {
                        sayln!("  {s}");
                    }
                }
                Show::Link => self.exit = self.link(),
            }
        }
        Vec::new()
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
                sayln!("[session {} · {}]", session.short(), workspace.display());
            }
            Change::Delta { call, text } => {
                if self.streaming != Some(*call) {
                    self.streaming = Some(*call);
                    say!("a    ");
                }
                say!("{text}");
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
                    sayln!();
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
                    sayln!("[{}]", recap_line(recap));
                }
                for call in view.ledger.inbox() {
                    if let Some(item) = view.items.iter().find(|i| i.key() == call.0) {
                        sayln!("{}", ask(&item.present()));
                    }
                }
            }
            Change::Mismatch { supported } => eprintln!("[{}]", mismatch_line(*supported)),
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
                    sayln!();
                } else if let Shown::Markdown(text) = p.body
                    && !text.is_empty()
                {
                    sayln!("a    {text}");
                }
                if let Some(usage) = p.usage {
                    sayln!("     · {usage}");
                }
                if !p.status.is_empty() {
                    sayln!("  └ {}", p.status);
                }
            }
            (Part::Outcome, Kind::Tool | Kind::Command | Kind::Child) => print_all(&outcome(&p)),
            (Part::Phase, _) if self.synced && p.buttons != protocol::view::Buttons::None => {
                sayln!("{}", ask(&p));
            }
            (Part::Answer { approved }, _) => {
                sayln!("  · {}", answer_line(approved));
            }
            _ => {}
        }
    }
}

fn print_all(lines: &[String]) {
    for line in lines {
        sayln!("{line}");
    }
}

/// The question a call waiting for approval puts.
fn ask(p: &Presented<'_>) -> String {
    format!("? {}  {}  [y/n]", p.title, p.status)
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

fn ls(daemon: &client::Daemon) -> io::Result<()> {
    for s in connect(daemon)?.sessions()? {
        sayln!("{s}");
    }
    Ok(())
}

fn new(daemon: &client::Daemon) -> io::Result<()> {
    let session = daemon.new_session().map_err(|e| wedged(daemon, e))?;
    sayln!("{}", session.short());
    Ok(())
}

#[cfg(feature = "web")]
fn web(daemon: client::Daemon, open: bool) -> io::Result<()> {
    web::run(daemon, open)
}

#[cfg(not(feature = "web"))]
fn web(_daemon: client::Daemon, _open: bool) -> io::Result<()> {
    Err(io::Error::other("this build of a has no web UI"))
}

/// The daemon. Started by a client (`listen_fd`), it leaves the client's
/// session and logs to `logs/daemon.log`; by hand, it stays at the
/// terminal.
fn serve(
    daemon: &client::Daemon,
    listen_fd: Option<i32>,
    lock_fd: Option<i32>,
    reset_breaker: bool,
) -> io::Result<()> {
    let home = daemon.home()?;
    sys::crash::install(home.crash_dir(), daemon.staged.id.clone());
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
        &daemon.staged.id,
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
                // The line printer prints an item as it is added, asks when it
                // asks, and ends it once settled: in between it says nothing.
                let mut repl = super::added(item);
                let asking = p.buttons != protocol::view::Buttons::None;
                if asking {
                    repl.push(super::ask(&p));
                }
                if item.settled() {
                    repl.extend(super::outcome(&p));
                }
                let repl = repl.join("\n");
                let repl_status = item.settled() || asking;
                // A reply's title is its model, shown with its usage, which the
                // terminal keeps in its status line.
                let title = if p.kind == Kind::Reply { "" } else { &p.title };
                let said = [
                    bare(title),
                    bare(&p.status),
                    match &p.body {
                        Shown::None => String::new(),
                        body => bare(body.text().lines().next().unwrap_or_default()),
                    },
                ];
                for (name, shown) in [("tui", &tui), ("web", &web), ("repl", &repl)] {
                    let shown = bare(shown);
                    for (n, word) in said.iter().enumerate() {
                        if n == 1 && name == "repl" && !repl_status {
                            continue;
                        }
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
