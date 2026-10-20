//! What a person asks a session client for, and the one table of slash
//! commands that every client (the terminal, the line REPL, the web page,
//! the apps through their FFI) parses typed lines with.

use protocol::{CallId, CommandInfo, Target};

/// A person's intent: from a typed line, a key or a button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// Input for the session: a message, or a slash command the daemon
    /// runs.
    Input(String),
    /// Input for the running turn, before its next step.
    Steer(String),
    Approve {
        call: CallId,
        approved: bool,
    },
    Interrupt,
    Detach,
    /// Sends this client's environment again.
    RefreshEnv,
    /// Asks for the session list, to pick from.
    Sessions,
    /// Attaches to another session (`/new` is [`Target::New`]).
    Open(Target),
    Help,
}

/// A slash command the client handles itself.
pub struct Command {
    pub name: &'static str,
    /// Other names for it.
    pub aliases: &'static [&'static str],
    /// What follows the name, as help shows it.
    pub args: &'static str,
    pub help: &'static str,
    /// The intent for what follows the name; `None` sends the line to the
    /// daemon as it is.
    parse: fn(&str) -> Option<Cmd>,
}

impl Command {
    /// How help shows it: `/name args`.
    pub fn usage(&self) -> String {
        if self.args.is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, self.args)
        }
    }
}

/// Every client's own slash commands. Any other slash line is input, for
/// the daemon's commands (which it advertises, see [`help`]).
pub const COMMANDS: &[Command] = &[
    Command {
        name: "sessions",
        aliases: &[],
        args: "",
        help: "switch session",
        parse: |rest| rest.is_empty().then_some(Cmd::Sessions),
    },
    Command {
        name: "new",
        aliases: &[],
        args: "",
        help: "start a new session",
        parse: |rest| rest.is_empty().then_some(Cmd::Open(Target::New)),
    },
    Command {
        name: "interrupt",
        aliases: &[],
        args: "",
        help: "stop the running turn",
        parse: |rest| rest.is_empty().then_some(Cmd::Interrupt),
    },
    Command {
        name: "steer",
        aliases: &[],
        args: "text",
        help: "tell the running turn, before its next step",
        parse: |rest| (!rest.is_empty()).then(|| Cmd::Steer(rest.to_owned())),
    },
    Command {
        name: "env",
        aliases: &[],
        args: "refresh",
        help: "send this shell's environment again",
        parse: |rest| (rest == "refresh").then_some(Cmd::RefreshEnv),
    },
    Command {
        name: "detach",
        aliases: &["quit", "exit"],
        args: "",
        help: "leave; the session keeps running",
        parse: |rest| rest.is_empty().then_some(Cmd::Detach),
    },
    Command {
        name: "help",
        aliases: &[],
        args: "",
        help: "keys and commands",
        parse: |rest| rest.is_empty().then_some(Cmd::Help),
    },
];

/// What `line` asks for; `None` for a blank one. While call `asking` waits
/// for this client's answer, `y`/`yes` and `n`/`no` answer it.
pub fn parse(line: &str, asking: Option<CallId>) -> Option<Cmd> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if let Some(call) = asking {
        match line {
            "y" | "yes" => {
                return Some(Cmd::Approve {
                    call,
                    approved: true,
                });
            }
            "n" | "no" => {
                return Some(Cmd::Approve {
                    call,
                    approved: false,
                });
            }
            _ => {}
        }
    }
    let own = line.strip_prefix('/').and_then(|slash| {
        let (name, rest) = slash.split_once(' ').unwrap_or((slash, ""));
        let command = COMMANDS
            .iter()
            .find(|c| c.name == name || c.aliases.contains(&name))?;
        (command.parse)(rest.trim())
    });
    Some(own.unwrap_or_else(|| Cmd::Input(line.to_owned())))
}

/// Every slash command, as `(usage, help)`: the client's own, then those
/// the daemon advertised that the client does not shadow.
pub fn help(daemon: &[CommandInfo]) -> impl Iterator<Item = (String, &str)> {
    let own = COMMANDS.iter().map(|c| (c.usage(), c.help));
    let theirs = daemon
        .iter()
        .filter(|d| !COMMANDS.iter().any(|c| c.name == d.name))
        .map(|d| (format!("/{}", d.name), d.description.as_str()));
    own.chain(theirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_table_is_the_only_parser() {
        let seven = CallId::new(7);
        let cases: &[(&str, Option<CallId>, Option<Cmd>)] = &[
            ("", None, None),
            ("   ", CallId::new(3), None),
            ("/detach", None, Some(Cmd::Detach)),
            ("/quit", None, Some(Cmd::Detach)),
            (" /exit ", None, Some(Cmd::Detach)),
            ("/interrupt", None, Some(Cmd::Interrupt)),
            ("/env refresh", None, Some(Cmd::RefreshEnv)),
            ("/env", None, Some(Cmd::Input("/env".into()))),
            ("/sessions", None, Some(Cmd::Sessions)),
            ("/new", None, Some(Cmd::Open(Target::New))),
            ("/help", None, Some(Cmd::Help)),
            (
                "/steer and tests",
                None,
                Some(Cmd::Steer("and tests".into())),
            ),
            ("/steer", None, Some(Cmd::Input("/steer".into()))),
            ("/model gpt", None, Some(Cmd::Input("/model gpt".into()))),
            ("/nope", None, Some(Cmd::Input("/nope".into()))),
            ("y", None, Some(Cmd::Input("y".into()))),
            (
                "y",
                seven,
                Some(Cmd::Approve {
                    call: seven.unwrap(),
                    approved: true,
                }),
            ),
            (
                "no",
                seven,
                Some(Cmd::Approve {
                    call: seven.unwrap(),
                    approved: false,
                }),
            ),
            (
                " hello there ",
                None,
                Some(Cmd::Input("hello there".into())),
            ),
        ];
        for (line, asking, want) in cases {
            assert_eq!(&parse(line, *asking), want, "{line:?}");
        }
    }

    #[test]
    fn help_lists_the_daemons_commands_after_the_clients_own() {
        let daemon = [
            CommandInfo {
                name: "model".into(),
                description: "switch model".into(),
            },
            CommandInfo {
                name: "help".into(),
                description: "shadowed".into(),
            },
        ];
        let listed: Vec<_> = help(&daemon).collect();
        assert_eq!(listed.len(), COMMANDS.len() + 1);
        assert_eq!(
            listed.last().unwrap(),
            &("/model".to_owned(), "switch model")
        );
        assert!(listed.iter().any(|(u, _)| u == "/steer text"));
    }
}
