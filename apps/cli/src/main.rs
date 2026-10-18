use std::io::{self, BufRead, IsTerminal, Write};
use std::mem;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use client::Endpoint;
use protocol::view::{
    Approval, Change, Item, Link, Part, SessionList, SessionView, Status, Usage, call_title,
    output_text, recap_line,
};
use protocol::{Card, ClientMsg, DaemonMsg, Outcome, SessionId, SessionSummary, Target};
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
            find(&ctx, &id).and_then(|id| session(ctx, Target::Id(id)))
        }
        Some(Cmd::Ls) => ls(&ctx),
        Some(Cmd::New) => new(&ctx),
        Some(Cmd::Bridge { token }) => {
            let token = token.as_deref().unwrap_or(&ctx.staged.id);
            client::bridge(&ctx.home, token)
        }
        Some(Cmd::Web { no_open }) => web(ctx, !no_open),
        Some(Cmd::Daemon { action }) => daemon_cmd(&ctx, &action),
        Some(Cmd::Serve { listen_fd }) => daemon(&ctx, listen_fd),
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
        let on_drain = || eprintln!("[waiting for the previous daemon to finish its work]");
        match client::connect(&self.endpoint, &self.staged, &on_drain) {
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut
                    && matches!(self.endpoint, Endpoint::Local(_)) =>
            {
                recover(&self.home, &self.staged)
            }
            result => result,
        }
    }
}

fn recover(home: &Home, staged: &Staged) -> io::Result<client::Connection> {
    let d = client::diagnose(home);
    let who = d.pid.map_or_else(
        || "the daemon".to_owned(),
        |pid| format!("the daemon (pid {pid})"),
    );
    eprintln!(
        "! {who} is not responding after {}s",
        client::HELLO_TIMEOUT.as_secs()
    );
    eprintln!("  log: {}", d.log.display());
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
            match d.pid {
                None => println!("not running"),
                Some(pid) => {
                    match client::connect(&ctx.endpoint, &ctx.staged, &|| {}).and_then(list) {
                        Ok(s) => println!("running (pid {pid}), {} sessions", s.len()),
                        Err(e) => println!("running (pid {pid}) but not responding: {e}"),
                    }
                }
            }
            println!("log: {}", d.log.display());
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
    let mut conn = ctx.connect()?;
    let (workspace, env) = conn.origin()?;
    conn.writer.send(&ClientMsg::Attach {
        target,
        workspace,
        since,
        env: Some(env),
    })?;
    Ok(conn)
}

/// The line-mode client. When the daemon drains for an upgrade, it
/// reconnects (starting the new build) and replays what it missed.
fn repl(ctx: Ctx, target: Target) -> io::Result<()> {
    let client::Connection {
        mut reader,
        writer,
        remote,
    } = attach(&ctx, target, 0)?;
    let remote = remote.is_some();
    let writer = Arc::new(Mutex::new(writer));
    let swap = Arc::clone(&writer);
    // Shared with the input loop, which answers the first call asked.
    let view = Arc::new(Mutex::new(SessionView::default()));
    let folding = Arc::clone(&view);
    thread::spawn(move || {
        let mut screen = Screen::default();
        let lock = || folding.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            while let Ok(Some(msg)) = reader.recv() {
                if let Some(update) = lock().fold(&msg) {
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
            ("/interrupt", _) => ClientMsg::Interrupt,
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
            ("y" | "yes", Some(call)) => ClientMsg::Approve {
                call,
                approved: true,
            },
            ("n" | "no", Some(call)) => ClientMsg::Approve {
                call,
                approved: false,
            },
            (text, _) => ClientMsg::Input {
                text: text.to_owned(),
            },
        };
        if let Err(e) = send(&msg) {
            eprintln!("[not sent: {e}]");
        }
    }
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
        Item::Hook { hook, decision, .. } => println!("  · hook {hook}: {decision:?}"),
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
            let (text, note) = match (output_text(output, card), output) {
                (Some(shown), _) => shown,
                (None, serde_json::Value::String(text)) => (text.clone(), String::new()),
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

fn list(
    client::Connection {
        mut reader,
        mut writer,
        ..
    }: client::Connection,
) -> io::Result<Vec<SessionSummary>> {
    writer.send(&ClientMsg::List)?;
    let mut list = SessionList::default();
    reader.until(|msg| list.fold(&msg).then(|| mem::take(&mut list.sessions)))
}

fn ls(ctx: &Ctx) -> io::Result<()> {
    for s in list(ctx.connect()?)? {
        println!(
            "{}  {:>4} entries  {}",
            s.id.short(),
            s.entries,
            s.workspace.display()
        );
    }
    Ok(())
}

fn find(ctx: &Ctx, prefix: &str) -> io::Result<SessionId> {
    let matches: Vec<_> = list(ctx.connect()?)?
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

#[cfg(unix)]
fn daemon(ctx: &Ctx, listen_fd: Option<i32>) -> io::Result<()> {
    let home = &ctx.home;
    let listener = if let Some(fd) = listen_fd {
        sys::detach_session()?;
        sys::adopt_listener(fd)?
    } else {
        home.bind()?
    };
    engine::serve(&listener, home, &engine::Config::from_env(), &ctx.staged.id)
}

#[cfg(not(unix))]
fn daemon(_ctx: &Ctx, _listen_fd: Option<i32>) -> io::Result<()> {
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
