use std::io::{self, BufRead};
use std::process::ExitCode;
use std::thread;

use protocol::{Body, ClientMsg, DaemonMsg, SessionId, Target};
use sys::Home;
use usage_rs::{Cli, Subcommands};

/// Agent engine: attach to durable sessions run by a per-user daemon.
#[derive(Cli)]
#[usage(bin = "a", version = "0.0.0")]
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
    #[usage(name = "__daemon", hide)]
    Daemon {
        #[usage(long)]
        listen_fd: Option<i32>,
    },
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
        Some(Cmd::Daemon { listen_fd }) => daemon(listen_fd),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("a: {e}");
            ExitCode::FAILURE
        }
    }
}

fn workspace() -> io::Result<std::path::PathBuf> {
    Ok(sys::workspace_of(&std::env::current_dir()?))
}

fn repl(target: Target) -> io::Result<()> {
    let (mut reader, mut writer) = client::connect(&Home::from_env()?)?;
    writer.send(&ClientMsg::Attach {
        target,
        workspace: workspace()?,
        since: 0,
    })?;

    thread::spawn(move || {
        while let Ok(Some(msg)) = reader.recv() {
            print_msg(&msg);
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

fn print_msg(msg: &DaemonMsg) {
    match msg {
        DaemonMsg::Attached { session, workspace } => {
            println!("[session {} · {}]", session.short(), workspace.display());
        }
        DaemonMsg::Entry(e) => match &e.body {
            Body::User { text } => println!("you  {text}"),
            Body::Assistant { text } => println!("a    {text}"),
        },
        DaemonMsg::Synced { .. } | DaemonMsg::Welcome { .. } | DaemonMsg::Sessions { .. } => {}
        DaemonMsg::Mismatch { supported } => eprintln!("[daemon speaks protocol {supported}]"),
        DaemonMsg::Error { message } => eprintln!("[error: {message}]"),
    }
}

fn sessions() -> io::Result<Vec<protocol::SessionSummary>> {
    let (mut reader, mut writer) = client::connect(&Home::from_env()?)?;
    writer.send(&ClientMsg::List)?;
    while let Some(msg) = reader.recv()? {
        if let DaemonMsg::Sessions { sessions } = msg {
            return Ok(sessions);
        }
    }
    Err(io::Error::other("daemon closed the connection"))
}

fn ls() -> io::Result<()> {
    for s in sessions()? {
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
    let matches: Vec<_> = sessions()?
        .into_iter()
        .filter(|s| {
            s.id.short().starts_with(prefix) || s.id.0.simple().to_string().starts_with(prefix)
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
    let (mut reader, mut writer) = client::connect(&Home::from_env()?)?;
    writer.send(&ClientMsg::Attach {
        target: Target::New,
        workspace: workspace()?,
        since: 0,
    })?;
    while let Some(msg) = reader.recv()? {
        if let DaemonMsg::Attached { session, .. } = msg {
            println!("{}", session.short());
            return writer.send(&ClientMsg::Detach);
        }
    }
    Err(io::Error::other("daemon closed the connection"))
}

#[cfg(unix)]
fn daemon(listen_fd: Option<i32>) -> io::Result<()> {
    let home = Home::from_env()?;
    let listener = if let Some(fd) = listen_fd {
        sys::detach_session()?;
        sys::adopt_listener(fd)?
    } else {
        home.prepare_socket_dir()?;
        let _ = std::fs::remove_file(home.socket());
        std::os::unix::net::UnixListener::bind(home.socket())?
    };
    engine::serve(&listener, &home)
}

#[cfg(not(unix))]
fn daemon(_listen_fd: Option<i32>) -> io::Result<()> {
    Err(io::Error::other(
        "the daemon is not implemented on this platform yet",
    ))
}
