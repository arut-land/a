//! The command line: one `const` table of commands, flags and arguments,
//! which `lexopt` reads argv against and which also writes the help, the
//! shell completions and the man page.

use std::ffi::OsString;
use std::fmt::Write;

use lexopt::{Arg as Lex, ValueExt};

#[cfg(test)]
mod generate;
#[cfg(test)]
mod tests;

pub const BIN: &str = "a";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a command runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Session,
    Resume,
    Attach,
    Ls,
    New,
    Bridge,
    Web,
    Daemon(Daemon),
    Serve,
}

/// What `a daemon` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Daemon {
    Status,
    Stop,
    Restart,
}

/// A command. One without an [`Op`] only groups its subcommands, so one of
/// them must follow.
pub struct Cmd {
    pub name: &'static str,
    pub help: &'static str,
    pub op: Option<Op>,
    pub flags: &'static [Flag],
    pub args: &'static [Arg],
    pub subs: &'static [Cmd],
    pub hidden: bool,
}

/// A `--long` flag: a switch, or one that takes the value named `value`.
/// The root's flags are global: they may follow any subcommand.
pub struct Flag {
    pub long: &'static str,
    pub value: Option<&'static str>,
    pub help: &'static str,
    /// The variable read when the flag is not given.
    pub env: Option<&'static str>,
}

/// A positional argument.
pub struct Arg {
    pub name: &'static str,
    pub required: bool,
    pub help: &'static str,
}

const CMD: Cmd = Cmd {
    name: "",
    help: "",
    op: None,
    flags: &[],
    args: &[],
    subs: &[],
    hidden: false,
};

const fn switch(long: &'static str, help: &'static str) -> Flag {
    Flag {
        long,
        value: None,
        help,
        env: None,
    }
}

const fn takes(long: &'static str, value: &'static str, help: &'static str) -> Flag {
    Flag {
        value: Some(value),
        ..switch(long, help)
    }
}

pub const HOST: Flag = Flag {
    env: Some("A_HOST"),
    ..takes(
        "host",
        "HOST",
        "Use the daemon on HOST, reached with `ssh HOST a bridge`",
    )
};
pub const ANY: Flag = switch("any", "Take the latest session across all workspaces");
pub const NO_OPEN: Flag = switch("no-open", "Only print the link; do not open a browser");
pub const LISTEN_FD: Flag = takes("listen-fd", "LISTEN_FD", "");
pub const LOCK_FD: Flag = takes(
    "lock-fd",
    "LOCK_FD",
    "The daemon lock, held for it by the client that started it",
);
pub const RESET_BREAKER: Flag = switch("reset-breaker", "Serve even after a crash loop");

/// `-h` and `--help`, which every command takes, and `-V` and
/// `--version`, which the root takes, as help lists them.
const HELP: (&str, &str) = ("-h, --help", "Print help");
const VERSION_FLAG: (&str, &str) = ("-V, --version", "Print version");
/// The `help` subcommand of every command that has subcommands.
const HELP_CMD: (&str, &str) = (
    "help",
    "Print this message or the help of the given subcommand(s)",
);

pub const ROOT: Cmd = Cmd {
    name: BIN,
    help: "Agent engine: attach to durable sessions run by a per-user daemon.",
    op: Some(Op::Session),
    flags: &[HOST],
    subs: &[
        Cmd {
            name: "resume",
            help: "Resume the latest session in this workspace, or pick one",
            op: Some(Op::Resume),
            flags: &[ANY],
            args: &[Arg {
                name: "ID",
                required: false,
                help: "",
            }],
            ..CMD
        },
        Cmd {
            name: "attach",
            help: "Attach to a session by id",
            op: Some(Op::Attach),
            args: &[Arg {
                name: "ID",
                required: true,
                help: "",
            }],
            ..CMD
        },
        Cmd {
            name: "ls",
            help: "List sessions",
            op: Some(Op::Ls),
            ..CMD
        },
        Cmd {
            name: "new",
            help: "Start a new session without attaching",
            op: Some(Op::New),
            ..CMD
        },
        Cmd {
            name: "bridge",
            help: "Pipe stdio to the local daemon (for `ssh host a bridge`)",
            op: Some(Op::Bridge),
            args: &[Arg {
                name: "TOKEN",
                required: false,
                help: "Printed in the `A-BRIDGE` line that marks the start of the stream",
            }],
            ..CMD
        },
        Cmd {
            name: "web",
            help: "Serve the web UI and open the browser",
            op: Some(Op::Web),
            flags: &[NO_OPEN],
            ..CMD
        },
        Cmd {
            name: "daemon",
            help: "Inspect or control the background daemon",
            subs: &[
                Cmd {
                    name: "status",
                    help: "Show whether the daemon is running and responding",
                    op: Some(Op::Daemon(Daemon::Status)),
                    ..CMD
                },
                Cmd {
                    name: "stop",
                    help: "Stop the daemon; sessions resume when it next starts",
                    op: Some(Op::Daemon(Daemon::Stop)),
                    ..CMD
                },
                Cmd {
                    name: "restart",
                    help: "Stop the daemon and start a fresh one",
                    op: Some(Op::Daemon(Daemon::Restart)),
                    ..CMD
                },
            ],
            ..CMD
        },
        Cmd {
            name: "__daemon",
            op: Some(Op::Serve),
            flags: &[LISTEN_FD, LOCK_FD, RESET_BREAKER],
            hidden: true,
            ..CMD
        },
    ],
    ..CMD
};

/// The command argv names, its flags and its arguments.
#[derive(Debug)]
pub struct Matches {
    pub op: Op,
    flags: Vec<(&'static str, Option<String>)>,
    args: Vec<String>,
}

impl Matches {
    pub fn on(&self, flag: &Flag) -> bool {
        self.flags.iter().any(|(long, _)| *long == flag.long)
    }

    /// The last value given for `flag`.
    pub fn value(&self, flag: &Flag) -> Option<&str> {
        let given = self.flags.iter().rev().find(|(long, _)| *long == flag.long);
        given.and_then(|(_, value)| value.as_deref())
    }

    pub fn arg(&self, at: usize) -> Option<&str> {
        self.args.get(at).map(String::as_str)
    }
}

/// Why parsing stopped short of a command: text for stdout (help asked
/// for), or for stderr with a usage error.
#[derive(Debug)]
pub enum Exit {
    Help(String),
    Usage(String),
}

/// Reads `argv` (without the binary's name), with `env` for the flags
/// that fall back to a variable.
pub fn parse(
    argv: impl IntoIterator<Item = OsString>,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<Matches, Exit> {
    let mut parser = lexopt::Parser::from_args(argv);
    let mut path = vec![&ROOT];
    let (mut flags, mut values) = (Vec::new(), Vec::new());
    let fail = |path: &[&Cmd], error: &dyn std::fmt::Display| {
        let mut text = format!("error: {error}\n\nUsage: ");
        usage(&mut text, path);
        text.push_str("\n\nFor more information, try '--help'.\n");
        Exit::Usage(text)
    };
    loop {
        let cmd = path[path.len() - 1];
        let arg = match parser.next() {
            Ok(Some(arg)) => arg,
            Ok(None) => break,
            Err(error) => return Err(fail(&path, &error)),
        };
        match arg {
            Lex::Short('h') | Lex::Long("help") => return Err(Exit::Help(help(&path))),
            Lex::Short('V') | Lex::Long("version") if path.len() == 1 => {
                return Err(Exit::Help(format!("{BIN} {VERSION}\n")));
            }
            Lex::Long(long) => {
                let Some(flag) = cmd.flags.iter().chain(ROOT.flags).find(|f| f.long == long) else {
                    return Err(fail(&path, &format_args!("unexpected argument '--{long}'")));
                };
                let value = flag
                    .value
                    .map(|_| parser.value().and_then(ValueExt::string));
                let value = value.transpose().map_err(|error| fail(&path, &error))?;
                flags.push((flag.long, value));
            }
            Lex::Short(short) => {
                return Err(fail(&path, &format_args!("unexpected argument '-{short}'")));
            }
            Lex::Value(value) => {
                let value = value.string().map_err(|error| fail(&path, &error))?;
                // A command takes subcommands or arguments, not both.
                if !cmd.subs.is_empty() && value == HELP_CMD.0 {
                    return Err(Exit::Help(help_of(&mut parser, path)));
                }
                match cmd.subs.iter().find(|sub| sub.name == value) {
                    Some(sub) => path.push(sub),
                    None if values.len() < cmd.args.len() => values.push(value),
                    None => {
                        return Err(fail(&path, &format_args!("unexpected argument '{value}'")));
                    }
                }
            }
        }
    }
    let cmd = path[path.len() - 1];
    let Some(op) = cmd.op else {
        return Err(Exit::Usage(help(&path)));
    };
    if let Some(arg) = cmd.args.get(values.len()).filter(|arg| arg.required) {
        let missing = format_args!("a required argument was not provided: <{}>", arg.name);
        return Err(fail(&path, &missing));
    }
    for flag in path.iter().flat_map(|cmd| cmd.flags) {
        let fallback = flag.env.and_then(&env).and_then(|v| v.into_string().ok());
        if let Some(value) =
            fallback.filter(|v| !v.is_empty() && !flags.iter().any(|f| f.0 == flag.long))
        {
            flags.push((flag.long, Some(value)));
        }
    }
    Ok(Matches {
        op,
        flags,
        args: values,
    })
}

/// `help [COMMAND]...`: the help of the command the rest of argv names.
fn help_of(parser: &mut lexopt::Parser, mut path: Vec<&'static Cmd>) -> String {
    while let Ok(Some(Lex::Value(name))) = parser.next() {
        let sub = path[path.len() - 1]
            .subs
            .iter()
            .find(|sub| *sub.name == name);
        match sub {
            Some(sub) => path.push(sub),
            None => break,
        }
    }
    help(&path)
}

/// The usage line of the command at the end of `path`.
fn usage(out: &mut String, path: &[&Cmd]) {
    let cmd = path[path.len() - 1];
    let names: Vec<_> = path.iter().map(|cmd| cmd.name).collect();
    out.push_str(&names.join(" "));
    for flag in cmd.flags {
        match flag.value {
            Some(value) => _ = write!(out, " [--{} <{value}>]", flag.long),
            None => _ = write!(out, " [--{}]", flag.long),
        }
    }
    for arg in cmd.args {
        let (open, close) = if arg.required { ('<', '>') } else { ('[', ']') };
        _ = write!(out, " {open}{}{close}", arg.name);
    }
    match (cmd.subs.is_empty(), cmd.op) {
        (true, _) => {}
        (false, Some(_)) => out.push_str(" [COMMAND]"),
        (false, None) => out.push_str(" <COMMAND>"),
    }
}

impl Flag {
    /// The flag as help lists it.
    fn shown(&self) -> String {
        match self.value {
            Some(value) => format!("    --{} <{value}>", self.long),
            None => format!("    --{}", self.long),
        }
    }

    fn described(&self) -> String {
        match self.env {
            Some(env) => format!("{} [env: {env}]", self.help),
            None => self.help.to_owned(),
        }
    }
}

/// The help of the command at the end of `path`.
pub fn help(path: &[&Cmd]) -> String {
    let cmd = path[path.len() - 1];
    let mut out = String::new();
    if path.len() == 1 {
        _ = writeln!(out, "{BIN} {VERSION}");
    }
    if !cmd.help.is_empty() {
        _ = writeln!(out, "{}\n", cmd.help);
    }
    out.push_str("Usage: ");
    usage(&mut out, path);
    out.push('\n');
    let subs = cmd.subs.iter().filter(|sub| !sub.hidden);
    let mut subs: Vec<_> = subs
        .map(|sub| (sub.name.to_owned(), sub.help.to_owned()))
        .collect();
    if !subs.is_empty() {
        subs.push((HELP_CMD.0.to_owned(), HELP_CMD.1.to_owned()));
    }
    section(&mut out, "Commands", &subs);
    let args = cmd.args.iter().filter(|arg| !arg.help.is_empty());
    let args: Vec<_> = args
        .map(|arg| (arg.name.to_owned(), arg.help.to_owned()))
        .collect();
    section(&mut out, "Arguments", &args);
    let mut own: Vec<_> = cmd
        .flags
        .iter()
        .map(|f| (f.shown(), f.described()))
        .collect();
    own.push((HELP.0.to_owned(), HELP.1.to_owned()));
    if path.len() == 1 {
        own.push((VERSION_FLAG.0.to_owned(), VERSION_FLAG.1.to_owned()));
    }
    section(&mut out, "Flags", &own);
    if path.len() > 1 {
        let global: Vec<_> = ROOT
            .flags
            .iter()
            .map(|f| (f.shown(), f.described()))
            .collect();
        section(&mut out, "Global flags", &global);
    }
    out
}

/// A titled list of `(name, help)` rows, the helps aligned.
fn section(out: &mut String, title: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        return;
    }
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    _ = writeln!(out, "\n{title}:");
    for (name, help) in rows {
        _ = writeln!(out, "  {name:width$}  {help}");
    }
}
