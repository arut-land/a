use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::thread;

use protocol::{Body, ClientMsg, DaemonMsg, Outcome, SessionId, SessionSummary, Target};
use sys::Home;
use usage_rs::{Cli, Subcommands};

/// Agent engine: attach to durable sessions run by a per-user daemon.
#[derive(Cli)]
#[usage(bin = "a", version = "0.0.0", spec_endpoint_file = "a.usage.kdl")]
struct Args {
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
    Bridge,
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
    let result = match Args::parse().cmd {
        None => repl(Target::New),
        Some(Cmd::Resume { id: None, any }) => repl(Target::Latest { any_workspace: any }),
        Some(Cmd::Resume { id: Some(id), .. } | Cmd::Attach { id }) => {
            find(&id).and_then(|id| repl(Target::Id(id)))
        }
        Some(Cmd::Ls) => ls(),
        Some(Cmd::New) => new(),
        Some(Cmd::Bridge) => Err(io::Error::other("bridge is not implemented yet")),
        Some(Cmd::Daemon { action }) => daemon_cmd(&action),
        Some(Cmd::Serve { listen_fd }) => daemon(listen_fd),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("a: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Connects to the daemon. A wedged daemon is diagnosed; at a terminal the
/// user can restart it on the spot, elsewhere `a daemon restart` is suggested.
fn connect() -> io::Result<client::Connection> {
    let home = Home::from_env()?;
    match client::connect(&home) {
        Err(e) if e.kind() == io::ErrorKind::TimedOut => recover(&home),
        result => result,
    }
}

fn recover(home: &Home) -> io::Result<client::Connection> {
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
            "r" => return client::restart(home),
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

fn daemon_cmd(action: &DaemonCmd) -> io::Result<()> {
    let home = Home::from_env()?;
    match action {
        DaemonCmd::Status => {
            let d = client::diagnose(&home);
            match d.pid {
                None => println!("not running"),
                Some(pid) => match client::connect(&home).and_then(list) {
                    Ok(s) => println!("running (pid {pid}), {} sessions", s.len()),
                    Err(e) => println!("running (pid {pid}) but not responding: {e}"),
                },
            }
            println!("log: {}", d.log.display());
        }
        DaemonCmd::Stop => {
            let stopped = client::stop(&home)?;
            println!("{}", if stopped { "stopped" } else { "not running" });
        }
        DaemonCmd::Restart => {
            client::restart(&home)?;
            println!("restarted");
        }
    }
    Ok(())
}

/// Connects and asks to attach to `target` from this directory's workspace.
fn attach(target: Target) -> io::Result<client::Connection> {
    let (reader, mut writer) = connect()?;
    writer.send(&ClientMsg::Attach {
        target,
        workspace: sys::workspace_of(&std::env::current_dir()?),
        since: 0,
    })?;
    Ok((reader, writer))
}

fn repl(target: Target) -> io::Result<()> {
    let (mut reader, mut writer) = attach(target)?;
    thread::spawn(move || {
        let mut view = View::default();
        while let Ok(Some(msg)) = reader.recv() {
            view.show(&msg);
        }
        eprintln!("[daemon disconnected]");
        std::process::exit(1);
    });

    for line in io::stdin().lock().lines() {
        let line = line?;
        match line.trim() {
            "" => {}
            "/detach" => break,
            text => writer.send(&ClientMsg::Input {
                text: text.to_owned(),
            })?,
        }
    }
    writer.send(&ClientMsg::Detach)?;
    println!("[detached; the session keeps running. `a resume` to reattach]");
    Ok(())
}

/// Renders daemon messages. A model reply streams in as deltas; on replay
/// there are none, so the committed text is printed instead.
#[derive(Default)]
struct View {
    streaming: Option<u64>,
}

impl View {
    fn show(&mut self, msg: &DaemonMsg) {
        match msg {
            DaemonMsg::Attached { session, workspace } => {
                println!("[session {} · {}]", session.short(), workspace.display());
            }
            DaemonMsg::Delta { call, text } => {
                if self.streaming != Some(*call) {
                    self.streaming = Some(*call);
                    print!("a    ");
                }
                print!("{text}");
                let _ = io::stdout().flush();
            }
            DaemonMsg::Entry(e) => self.entry(&e.body),
            DaemonMsg::Synced { .. } | DaemonMsg::Welcome { .. } | DaemonMsg::Sessions { .. } => {}
            DaemonMsg::Mismatch { supported } => eprintln!("[daemon speaks protocol {supported}]"),
            DaemonMsg::Error { message } => eprintln!("[error: {message}]"),
        }
    }

    fn entry(&mut self, body: &Body) {
        match body {
            Body::User { text } => println!("you  {text}"),
            Body::Assistant { text } => println!("a    {text}"),
            Body::ToolCall { tool, input, .. } => match input["command"].as_str() {
                Some(command) => println!("tool $ {command}"),
                None => println!("tool {tool} {input}"),
            },
            Body::ToolResult { outcome, .. } => print_outcome(outcome),
            Body::ModelResult { call, outcome } => {
                // A streamed reply is on screen already; end its line.
                if self.streaming.take() == Some(*call) {
                    println!();
                } else if let Outcome::Done { output } = outcome
                    && let text = reply_text(output)
                    && !text.is_empty()
                {
                    println!("a    {text}");
                }
                match outcome {
                    Outcome::Done { output } => {
                        let usage = &output["usage"];
                        if let (Some(input), Some(out)) =
                            (usage["input"].as_u64(), usage["output"].as_u64())
                        {
                            println!("     · {input} in, {out} out");
                        }
                    }
                    Outcome::Failed { error } => println!("  └ model failed: {error}"),
                    Outcome::Interrupted => println!("  └ model call interrupted"),
                }
            }
            Body::Hook { hook, decision } => println!("  · hook {hook}: {decision:?}"),
            Body::ToolProcess { .. } | Body::ModelCall { .. } => {}
        }
    }
}

/// The answer text of a stored model reply: its message's text parts, or
/// the bare text an earlier version logged.
fn reply_text(output: &serde_json::Value) -> String {
    match output
        .pointer("/message/content")
        .and_then(serde_json::Value::as_array)
    {
        Some(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect(),
        None => output["text"].as_str().unwrap_or_default().to_owned(),
    }
}

fn print_outcome(outcome: &Outcome) {
    match outcome {
        Outcome::Done { output } => {
            if let Some(stdout) = output["stdout"].as_str() {
                let stderr = output["stderr"].as_str().unwrap_or_default();
                for line in stdout.lines().chain(stderr.lines()) {
                    println!("  │ {line}");
                }
                println!("  └ exit {}", output["exit"]);
            } else {
                println!("  └ {output}");
            }
        }
        Outcome::Failed { error } => println!("  └ failed: {error}"),
        Outcome::Interrupted => println!("  └ interrupted: the daemon stopped mid-call"),
    }
}

fn list((mut reader, mut writer): client::Connection) -> io::Result<Vec<SessionSummary>> {
    writer.send(&ClientMsg::List)?;
    reader.until(|msg| match msg {
        DaemonMsg::Sessions { sessions } => Some(sessions),
        _ => None,
    })
}

fn ls() -> io::Result<()> {
    for s in list(connect()?)? {
        println!(
            "{}  {:>4} entries  {}",
            s.id.short(),
            s.entries,
            s.workspace.display()
        );
    }
    Ok(())
}

fn find(prefix: &str) -> io::Result<SessionId> {
    let matches: Vec<_> = list(connect()?)?
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

fn new() -> io::Result<()> {
    let (mut reader, mut writer) = attach(Target::New)?;
    let session = reader.until(|msg| match msg {
        DaemonMsg::Attached { session, .. } => Some(session),
        _ => None,
    })?;
    println!("{}", session.short());
    writer.send(&ClientMsg::Detach)
}

#[cfg(unix)]
fn daemon(listen_fd: Option<i32>) -> io::Result<()> {
    let home = Home::from_env()?;
    let listener = if let Some(fd) = listen_fd {
        sys::detach_session()?;
        sys::adopt_listener(fd)?
    } else {
        home.bind()?
    };
    engine::serve(&listener, &home, &engine::Config::from_env())
}

#[cfg(not(unix))]
fn daemon(_listen_fd: Option<i32>) -> io::Result<()> {
    Err(io::Error::other(
        "the daemon is not implemented on this platform yet",
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::Args;

    /// `a.usage.kdl` is checked in so the binary does not link the spec
    /// writer; regenerate it with `mise run generate:usage`.
    #[test]
    fn embedded_spec_matches_the_cli() {
        let request = [OsStr::new(usage_rs::SPEC_REQUEST)];
        assert_eq!(
            Args::spec_request(&request).as_deref(),
            Some(Args::to_kdl().as_str())
        );
    }
}
