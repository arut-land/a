use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use client::Endpoint;
use protocol::view::{
    Approval, Change, Item, Link, Part, SessionView, Status, Usage, call_title, decision_line,
    output_text, recap_line,
};
use protocol::{Card, ClientMsg, DaemonMsg, Outcome, SessionId, Target};
use sys::{Home, Staged};
use usage_rs::{Cli, Subcommands};

/// Agent engine: attach to durable sessions run by a per-user daemon.
#[derive(Cli)]
#[usage(bin = "a", version = "0.0.0", spec_endpoint_file = "a.usage.kdl")]
struct Args {
    /// Use the daemon on HOST, reached with `ssh HOST a bridge`
    #[usage(long, global, env = "A_HOST", value_name = "HOST")]
    host: Option<String>,
    #[usage(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommands)]
enum Cmd {
    /// Resume the latest session in this workspace, or pick one
    Resume {
        id: Option<String>,
        /// Take the latest session across all workspaces
        #[usage(long)]
        any: bool,
    },
    /// Attach to a session by id
    Attach { id: String },
    /// List sessions
    Ls,
    /// Start a new session without attaching
    New,
    /// Pipe stdio to the local daemon (for `ssh host a bridge`)
    Bridge {
        /// Printed in the `A-BRIDGE` line that marks the start of the stream
        token: Option<String>,
    },
    /// Serve the web UI and open the browser
    Web {
        /// Only print the link; do not open a browser
        #[usage(long)]
        no_open: bool,
    },
    /// Inspect or control the background daemon
    Daemon {
        #[usage(subcommand)]
        action: DaemonCmd,
    },
    #[usage(name = "__daemon", hide)]
    Serve {
        #[usage(long)]
        listen_fd: Option<i32>,
        /// The daemon lock, held for it by the client that started it
        #[usage(long)]
        lock_fd: Option<i32>,
        /// Serve even after a crash loop
        #[usage(long)]
        reset_breaker: bool,
    },
}

#[derive(Subcommands)]
enum DaemonCmd {
    /// Show whether the daemon is running and responding
    Status,
    /// Stop the daemon; sessions resume when it next starts
    Stop,
    /// Stop the daemon and start a fresh one
    Restart,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = Ctx::new(args.host).and_then(|ctx| match args.cmd {
        None => session(ctx, Target::New),
        Some(Cmd::Resume { id: None, any }) => session(ctx, Target::Latest { any_workspace: any }),
        Some(Cmd::Resume { id: Some(id), .. } | Cmd::Attach { id }) => {
            target(&ctx, id).and_then(|target| session(ctx, target))
        }
        Some(Cmd::Ls) => ls(&ctx),
        Some(Cmd::New) => new(&ctx),
        Some(Cmd::Bridge { token }) => {
            let token = token.as_deref().unwrap_or(&ctx.staged.id);
            client::bridge(&ctx.home, token)
        }
        Some(Cmd::Web { no_open }) => web(ctx, !no_open),
        Some(Cmd::Daemon { action }) => daemon_cmd(&ctx, &action),
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

    /// Connects to the daemon. A wedged local daemon is diagnosed; at a
    /// terminal the user can restart it on the spot, elsewhere `a daemon
    /// restart` is suggested.
    fn connect(&self) -> io::Result<client::Connection> {
        self.connect_with(&|_| Ok(Vec::new()))
    }

    /// [`Ctx::connect`], sending what `first` makes right behind the hello.
    fn connect_with(
        &self,
        first: &dyn Fn(&client::Connection) -> io::Result<Vec<ClientMsg>>,
    ) -> io::Result<client::Connection> {
        let on_drain = || eprintln!("[waiting for the previous daemon to finish its work]");
        match client::connect_with(&self.endpoint, &self.staged, &on_drain, first) {
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut
                    && matches!(self.endpoint, Endpoint::Local(_)) =>
            {
                let mut conn = recover(&self.home, &self.staged)?;
                conn.send_all(&first(&conn)?)?;
                Ok(conn)
            }
            result => result,
        }
    }
}

fn recover(home: &Home, staged: &Staged) -> io::Result<client::Connection> {
    let d = client::diagnose(home);
    eprintln!(
        "! {} is not responding after {}s",
        d.who(),
        client::HELLO_TIMEOUT.as_secs()
    );
    eprintln!("  log: {}", d.log.display());
    if let Some(note) = d.crash_note() {
        eprintln!("  {note}");
    }
    if !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "run `a daemon restart` to replace it",
        ));
    }
    loop {
        eprint!("  [r] restart daemon  [l] show log  [q] quit: ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        match answer.trim() {
            "r" => return client::restart(home, staged),
            "l" => {
                for line in d.log_tail() {
                    eprintln!("  | {line}");
                }
            }
            "q" | "" => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "daemon not responding",
                ));
            }
            _ => {}
        }
    }
}

fn daemon_cmd(ctx: &Ctx, action: &DaemonCmd) -> io::Result<()> {
    let home = ctx.local()?;
    match action {
        DaemonCmd::Status => {
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
        DaemonCmd::Stop => {
            let stopped = client::stop(home)?;
            println!("{}", if stopped { "stopped" } else { "not running" });
        }
        DaemonCmd::Restart => {
            client::restart(home, &ctx.staged)?;
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
        None => repl(ctx, target),
    }
}

/// Connects and asks to attach to `target` from this directory's workspace
/// (or a remote daemon's), with this shell's environment (or the remote
/// bridge's), replaying entries after `since`.
fn attach(ctx: &Ctx, target: Target, since: u64) -> io::Result<client::Connection> {
    ctx.connect_with(&move |conn| Ok(vec![conn.attach(target.clone(), since, true)?]))
}

/// The line-mode client. When the daemon drains for an upgrade, it
/// reconnects (starting the new build) and replays what it missed.
fn repl(ctx: Ctx, target: Target) -> io::Result<()> {
    let client::Connection {
        mut reader,
        writer,
        remote,
        ..
    } = attach(&ctx, target, 0)?;
    let remote = remote.is_some();
    let writer = Arc::new(Mutex::new(writer));
    let swap = Arc::clone(&writer);
    // Set before the detach goes out: the daemon closing the connection
    // then is the answer, not a lost daemon.
    let detaching = Arc::new(AtomicBool::new(false));
    let detached = Arc::clone(&detaching);
    // Shared with the input loop, which answers the first call asked.
    let view = Arc::new(Mutex::new(SessionView::default()));
    let folding = Arc::clone(&view);
    thread::spawn(move || {
        let mut screen = Screen::default();
        let lock = || folding.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            while let Ok(Some(msg)) = reader.recv() {
                let mut view = lock();
                // Nothing to attach to: no such session.
                if let (None, DaemonMsg::Error { message }) = (view.session, &msg) {
                    eprintln!("a: {message}");
                    std::process::exit(1);
                }
                if let Some(update) = view.fold(&msg) {
                    screen.show(&update.change);
                }
            }
            let (session, since) = {
                let view = lock();
                (
                    view.session.filter(|_| view.link == Link::Draining),
                    view.seq,
                )
            };
            if detached.load(Ordering::SeqCst) {
                return;
            }
            let Some(session) = session else {
                eprintln!("[daemon disconnected]");
                std::process::exit(1);
            };
            eprintln!("[daemon upgraded; reconnecting]");
            let mut writer = swap.lock().unwrap_or_else(PoisonError::into_inner);
            match attach(&ctx, Target::Id(session), since) {
                Ok(conn) => (reader, *writer) = (conn.reader, conn.writer),
                Err(e) => {
                    eprintln!("[reconnect failed: {e}]");
                    std::process::exit(1);
                }
            }
        }
    });

    let send = |msg: &ClientMsg| {
        writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(msg)
    };
    for line in io::stdin().lock().lines() {
        let line = line?;
        let asked = view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .asked()
            .next();
        let msg = match (line.trim(), asked) {
            ("", _) => continue,
            ("/detach", _) => break,
            ("/interrupt", _) => ClientMsg::INTERRUPT,
            // A remote host's environment is the one its bridge started
            // with; reattaching captures it again.
            ("/env refresh", _) if remote => {
                eprintln!(
                    "[the host's environment is captured when `a` connects; reattach to refresh it]"
                );
                continue;
            }
            ("/env refresh", _) => match client::capture() {
                Ok(env) => ClientMsg::Environment(env),
                Err(e) => {
                    eprintln!("[not refreshed: {e}]");
                    continue;
                }
            },
            ("y" | "yes", Some(call)) => ClientMsg::approve(call, true),
            ("n" | "no", Some(call)) => ClientMsg::approve(call, false),
            (text, _) => ClientMsg::input(text),
        };
        if let Err(e) = send(&msg) {
            eprintln!("[not sent: {e}]");
        }
    }
    detaching.store(true, Ordering::SeqCst);
    send(&ClientMsg::Detach)?;
    println!("[detached; the session keeps running. `a resume` to reattach]");
    Ok(())
}

/// Prints the shared view's updates as transcript lines. A model reply
/// streams in as deltas; on replay there are none, so the committed text is
/// printed instead.
#[derive(Default)]
struct Screen {
    /// The call whose streamed reply is on the current line.
    streaming: Option<u64>,
}

impl Screen {
    fn show(&mut self, change: &Change) {
        match change {
            Change::Attached {
                session, workspace, ..
            } => println!("[session {} · {}]", session.short(), workspace.display()),
            Change::Delta { call, text } => {
                if self.streaming != Some(*call) {
                    self.streaming = Some(*call);
                    print!("a    ");
                }
                print!("{text}");
                let _ = io::stdout().flush();
            }
            Change::Added { item, .. } => added(item),
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
            Change::Asked {
                item: Item::Tool {
                    tool, input, card, ..
                },
                ..
            } => println!(
                "? approve {}  [y/n]",
                call_title(tool, input, card.as_ref())
            ),
            Change::Recap(recap) => println!("[{}]", recap_line(recap)),
            Change::Mismatch { supported } => eprintln!("[daemon speaks protocol {supported}]"),
            Change::Error { message } => eprintln!("[error: {message}]"),
            Change::Asked { .. } | Change::Synced | Change::Draining => {}
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

fn added(item: &Item) {
    match item {
        Item::User { text, .. } => println!("you  {text}"),
        Item::Assistant { text, .. } => println!("a    {text}"),
        Item::Tool {
            tool, input, card, ..
        } => println!("tool {}", call_title(tool, input, card.as_ref())),
        Item::Command { name, args, .. } => println!("cmd  /{name} {args}"),
        Item::Hook { hook, decision, .. } => {
            println!("  · hook {hook}: {}", decision_line(decision));
        }
        Item::Child { child, input, .. } => println!("child {} {input}", child.short()),
        Item::Environment { cwd, .. } => println!("  · env {}", cwd.display()),
        Item::Reply { .. } => {}
    }
}

/// An outcome as its tool's card presents it, else a command's streams as
/// older logs hold them, text as lines, and anything else as JSON.
fn print_outcome(outcome: &Outcome, card: Option<&Card>) {
    match outcome {
        Outcome::Done { output } => {
            let (text, note) = match (output_text(output, card), output.value()) {
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
        println!(
            "{}  {:<9}  {:<10}  {:>4} entries  {}",
            s.id.short(),
            s.activity.name(),
            s.residency.name(),
            s.entries,
            s.workspace.display()
        );
    }
    Ok(())
}

/// The session `id` names, in full or as a prefix: this machine's daemon
/// resolves it as it attaches; another machine's, which may be older, is
/// asked for its list first.
fn target(ctx: &Ctx, id: String) -> io::Result<Target> {
    match ctx.endpoint {
        Endpoint::Local(_) => Ok(Target::Prefix(id)),
        Endpoint::Ssh(_) => find(ctx, &id).map(Target::Id),
    }
}

fn find(ctx: &Ctx, prefix: &str) -> io::Result<SessionId> {
    let matches: Vec<_> = ctx
        .connect()?
        .sessions()?
        .into_iter()
        .filter(|s| {
            s.id.short().to_string().starts_with(prefix) || s.id.to_string().starts_with(prefix)
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.id),
        [] => Err(io::Error::other(format!("no session matches {prefix}"))),
        _ => Err(io::Error::other(format!(
            "{prefix} matches several sessions"
        ))),
    }
}

fn new(ctx: &Ctx) -> io::Result<()> {
    let mut conn = attach(ctx, Target::New, 0)?;
    let session = conn.reader.until(|msg| match msg {
        DaemonMsg::Attached { session, .. } => Some(session),
        _ => None,
    })?;
    println!("{}", session.short());
    conn.writer.send(&ClientMsg::Detach)
}

#[cfg(all(unix, feature = "web"))]
fn web(ctx: Ctx, open: bool) -> io::Result<()> {
    let daemon = web::Daemon {
        endpoint: ctx.endpoint,
        staged: ctx.staged,
    };
    web::run(daemon, open)
}

#[cfg(not(all(unix, feature = "web")))]
fn web(_ctx: Ctx, _open: bool) -> io::Result<()> {
    Err(io::Error::other("this build of a has no web UI"))
}

/// The daemon. Started by a client (`listen_fd`), it leaves the client's
/// session and logs to `logs/daemon.log`; by hand, it stays at the
/// terminal.
#[cfg(unix)]
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
        sys::adopt_listener(fd)?
    } else {
        home.bind()?
    };
    let start = engine::Start {
        lock: lock_fd.map(sys::adopt_lock).transpose()?,
        reset_breaker,
        logs: listen_fd.is_some(),
    };
    engine::serve(
        &listener,
        home,
        &engine::Config::load(home),
        &ctx.staged.id,
        start,
    )
}

#[cfg(not(unix))]
fn daemon(
    _ctx: &Ctx,
    _listen_fd: Option<i32>,
    _lock_fd: Option<i32>,
    _reset_breaker: bool,
) -> io::Result<()> {
    Err(io::Error::other(
        "the daemon is not implemented on this platform yet",
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::Args;

    /// `a.usage.kdl` is checked in so the binary does not link the spec
    /// writer; `mise run generate:usage` rewrites it (`UPDATE_USAGE=1`).
    #[test]
    fn embedded_spec_matches_the_cli() {
        if std::env::var_os("UPDATE_USAGE").is_some() {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/a.usage.kdl");
            std::fs::write(path, Args::to_kdl()).unwrap();
            return;
        }
        let request = [OsStr::new(usage_rs::SPEC_REQUEST)];
        assert_eq!(
            Args::spec_request(&request).as_deref(),
            Some(Args::to_kdl().as_str())
        );
    }
}
