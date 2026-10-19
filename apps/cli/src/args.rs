//! The command line, declared with usage-rs: its derive is the spec, and
//! the parser, help, errors (exit status 2) and the shell completions'
//! answers all read the tables it writes.

use usage_rs::{Cli, Subcommands};

#[cfg(test)]
mod generate;
#[cfg(test)]
mod tests;

/// Agent engine: attach to durable sessions run by a per-user daemon.
// No spec endpoint: nothing asks the binary for its spec, and answering
// would link the KDL writer.
#[derive(Debug, Cli)]
#[usage(bin = "a", version, spec_endpoint = false, completion)]
pub struct Args {
    /// Use the daemon on HOST, reached with `ssh HOST a bridge`
    #[usage(long, global, env = "A_HOST", value_name = "HOST")]
    pub host: Option<String>,
    #[usage(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Debug, PartialEq, Eq, Subcommands)]
pub enum Cmd {
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
        action: Daemon,
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

/// What `a daemon` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Subcommands)]
pub enum Daemon {
    /// Show whether the daemon is running and responding
    Status,
    /// Stop the daemon; sessions resume when it next starts
    Stop,
    /// Stop the daemon and start a fresh one
    Restart,
}
