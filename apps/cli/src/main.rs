mod args;
mod exts;

use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::thread;

use client::{Driver, End, Endpoint, Env, Event, Link, Renderer, SessionClient, Show};
use protocol::Verdict;
use protocol::view::SessionView;
use protocol::view::{
    Approval, Change, Item, Part, Phase, Status, Usage, call_title, decision_line, output_text,
    recap_line,
};
use protocol::{Card, Outcome, Target};
use sys::Home;

use crate::args::{Args, Cmd, Daemon};

fn main() -> ExitCode {
    // Help, the version, completion requests and usage errors (status 2)
    // end the process here.
    let args = Args::parse();
    let result = daemon_of(args.host).and_then(|daemon| match args.cmd {
        None => session(&daemon, Target::New),
        Some(Cmd::Resume { id: None, any }) => {
            session(&daemon, Target::Latest { any_workspace: any })
        }
        Some(Cmd::Resume { id: Some(id), .. } | Cmd::Attach { id }) => {
            session(&daemon, Target::Prefix(id))
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
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("a: {e}");
            ExitCode::FAILURE
        }
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
            daemon.restart()?;
            println!("restarted");
        }
    }
    Ok(())
}

/// The full-screen client at a terminal, the line REPL anywhere else (or
/// with `A_TUI=plain`).
fn session(daemon: &client::Daemon, target: Target) -> io::Result<()> {
    match tui::viewport() {
        Some(viewport) => tui::run(daemon, target, viewport),
        None => repl(daemon, target),
    }
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
                    println!("  {usage:<16} {help}");
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
                println!("[detached; the session keeps running. `a resume` to reattach]");
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
                        println!("  {s}");
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

/// Prints the shared view's updates as transcript lines. A model reply
/// streams in as deltas; on replay there are none, so the committed text is
/// printed instead. What waits for an approval is asked once the replay is
/// in, after its recap, and as it begins to wait from then on.
#[derive(Default)]
struct Screen {
    /// The call whose streamed reply is on the current line.
    streaming: Option<u64>,
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
                added(item);
            }
            // Sent in place of a long replay: shown as the replay would be.
            Change::Snapshot { items } => {
                for item in items {
                    added(item);
                    for part in [Part::Approval, Part::Outcome] {
                        self.updated(item, part);
                    }
                }
            }
            Change::Updated { item, part, .. } => self.updated(item, *part),
            Change::Synced => {
                self.synced = true;
                if let Some(recap) = &view.recap {
                    println!("[{}]", recap_line(recap));
                }
                for item in view
                    .items
                    .iter()
                    .filter(|i| i.phase() == Some(Phase::Asking))
                {
                    ask(item);
                }
            }
            Change::Mismatch { supported } => eprintln!("[daemon speaks protocol {supported}]"),
            Change::Error { message } => eprintln!("[error: {message}]"),
            Change::Draining => {}
        }
    }

    fn updated(&mut self, item: &Item, part: Part) {
        match (item, part) {
            (
                Item::Tool {
                    outcome: Some(outcome),
                    card,
                    ..
                },
                Part::Outcome,
            ) => print_outcome(outcome, card.as_ref()),
            (
                Item::Child {
                    fired: Some(outcome),
                    ..
                },
                Part::Outcome,
            ) => print_outcome(outcome, None),
            (Item::Tool { .. }, Part::Approval)
                if self.synced && item.phase() == Some(Phase::Asking) =>
            {
                ask(item);
            }
            (
                Item::Tool {
                    approval: Some(answer @ (Approval::Approved | Approval::Denied)),
                    ..
                },
                Part::Approval,
            ) => println!(
                "  · {}",
                if *answer == Approval::Approved {
                    "approved"
                } else {
                    "not approved"
                }
            ),
            (
                Item::Reply {
                    call,
                    text,
                    status,
                    usage,
                    ..
                },
                Part::Outcome,
            ) => {
                // A streamed reply is on screen already; end its line.
                if self.streaming.take() == Some(*call) {
                    println!();
                } else if *status == Status::Done && !text.is_empty() {
                    println!("a    {text}");
                }
                match status {
                    Status::Done => {
                        if let Some(Usage { input, output }) = usage {
                            println!("     · {input} in, {output} out");
                        }
                    }
                    Status::Failed { error } => println!("  └ model failed: {error}"),
                    Status::Interrupted => println!("  └ model call interrupted"),
                    Status::Running => {}
                }
            }
            _ => {}
        }
    }
}

fn ask(item: &Item) {
    if let Item::Tool {
        tool, input, card, ..
    } = item
    {
        println!(
            "? approve {}  [y/n]",
            call_title(tool, input, card.as_ref())
        );
    }
}

fn added(item: &Item) {
    match item {
        Item::User { text, .. } => println!("you  {text}"),
        Item::Assistant { text, .. } => println!("a    {text}"),
        Item::Tool {
            tool,
            input,
            by: protocol::Audience::User,
            ..
        } => println!("cmd  /{tool} {}", Item::args(input)),
        Item::Tool {
            tool, input, card, ..
        } => println!("tool {}", call_title(tool, input, card.as_ref())),
        Item::Hook {
            hook,
            call,
            tool,
            decision,
            ..
        } => println!("  · hook {hook}: {}", decision_line(tool, *call, decision)),
        Item::Child { child, input, .. } => println!("child {} {input}", child.short()),
        Item::Environment { cwd, .. } => println!("  · env {}", cwd.display()),
        Item::Reply { .. } => {}
    }
}

/// An outcome as its tool's card presents it, else text as lines and
/// anything else as JSON.
fn print_outcome(outcome: &Outcome, card: Option<&Card>) {
    match outcome {
        Outcome::Done { output } => {
            let (text, note) = match (output_text(card), output.parse::<String>()) {
                (Some(shown), _) => shown,
                (None, Ok(text)) => (text, String::new()),
                (None, Err(_)) => (String::new(), output.to_string()),
            };
            for line in text.lines() {
                println!("  │ {line}");
            }
            println!("  └ {}", if note.is_empty() { "done" } else { &note });
        }
        Outcome::Failed { error } => println!("  └ failed: {error}"),
        Outcome::Interrupted { output } => {
            for line in output.iter().flat_map(|text| text.lines()) {
                println!("  │ {line}");
            }
            println!("  └ interrupted before it finished");
        }
    }
}

fn ls(daemon: &client::Daemon) -> io::Result<()> {
    for s in connect(daemon)?.sessions()? {
        println!("{s}");
    }
    Ok(())
}

fn new(daemon: &client::Daemon) -> io::Result<()> {
    let session = daemon.new_session().map_err(|e| wedged(daemon, e))?;
    println!("{}", session.short());
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
