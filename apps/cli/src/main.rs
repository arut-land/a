mod args;
mod exts;

use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::thread;

use client::{Attach, Driver, Effect, End, Endpoint, Env, Event, Link, SessionClient};
use protocol::Verdict;
use protocol::view::SessionView;
use protocol::view::{
    Approval, Change, Item, Part, Phase, Status, Usage, call_title, decision_line, output_text,
    recap_line,
};
use protocol::{Card, ClientMsg, DaemonMsg, Outcome, SessionSummary, Target};
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
                }
                | Item::Command {
                    outcome: Some(outcome),
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
            tool, input, card, ..
        } => println!("tool {}", call_title(tool, input, card.as_ref())),
        Item::Command { name, args, .. } => println!("cmd  /{name} {args}"),
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
            let (text, note) = match (output_text(card), output.value()) {
                (Some(shown), _) => shown,
                (None, serde_json::Value::String(text)) => (text, String::new()),
                (None, output) => (String::new(), output.to_string()),
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

fn ls(ctx: &Ctx) -> io::Result<()> {
    for s in ctx.connect()?.sessions()? {
        println!("{}", listed(&s));
    }
    Ok(())
}

/// A session as `a ls` lists it.
fn listed(s: &SessionSummary) -> String {
    format!(
        "{}  {:<9}  {:<10}  {:>4} entries  {}",
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
