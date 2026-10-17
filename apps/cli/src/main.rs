use std::process::ExitCode;

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
    let what = match Args::parse().cmd {
        None => "new session".to_owned(),
        Some(Cmd::Resume { id, any }) => format!("resume id={id:?} any={any}"),
        Some(Cmd::Attach { id }) => format!("attach {id}"),
        Some(Cmd::Ls) => "ls".to_owned(),
        Some(Cmd::New) => "new".to_owned(),
        Some(Cmd::Bridge) => "bridge".to_owned(),
        Some(Cmd::Daemon { listen_fd }) => format!("__daemon listen_fd={listen_fd:?}"),
    };
    eprintln!("a: {what} is not implemented yet");
    ExitCode::from(2)
}
