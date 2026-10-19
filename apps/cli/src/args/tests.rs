//! The const table is the command line: these hold it to the shape the
//! parser, the help and the generators assume, and argv to the commands
//! it names.

use std::ffi::OsString;

use super::{ANY, Cmd, Daemon, Exit, HOST, LISTEN_FD, LOCK_FD, Matches, Op, RESET_BREAKER, ROOT};

fn parse(argv: &str) -> Result<Matches, Exit> {
    super::parse(argv.split_whitespace().map(OsString::from), |_| None)
}

fn op(argv: &str) -> Op {
    parse(argv).unwrap_or_else(|e| panic!("{argv}: {e:?}")).op
}

fn usage_error(argv: &str) -> String {
    match parse(argv) {
        Err(Exit::Usage(text)) => text,
        other => panic!("{argv}: {other:?}"),
    }
}

fn help(argv: &str) -> String {
    match parse(argv) {
        Err(Exit::Help(text)) => text,
        other => panic!("{argv}: {other:?}"),
    }
}

/// Every command, as the path from the root that names it.
pub(super) fn commands() -> Vec<Vec<&'static Cmd>> {
    fn walk(path: &mut Vec<&'static Cmd>, out: &mut Vec<Vec<&'static Cmd>>) {
        out.push(path.clone());
        for sub in path[path.len() - 1].subs {
            path.push(sub);
            walk(path, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(&mut vec![&ROOT], &mut out);
    out
}

#[test]
fn the_table_is_well_formed() {
    for path in commands() {
        let cmd = path[path.len() - 1];
        let at = path.iter().map(|c| c.name).collect::<Vec<_>>().join(" ");
        assert!(cmd.hidden || !cmd.help.is_empty(), "{at}: no help");
        assert!(
            cmd.op.is_some() || !cmd.subs.is_empty(),
            "{at}: runs nothing"
        );
        // A value names a subcommand or is an argument, never either.
        assert!(cmd.subs.is_empty() || cmd.args.is_empty(), "{at}");
        let first_optional = cmd.args.iter().position(|arg| !arg.required);
        let rest = &cmd.args[first_optional.unwrap_or(cmd.args.len())..];
        assert!(
            rest.iter().all(|arg| !arg.required),
            "{at}: required after optional"
        );
        let mut subs: Vec<_> = cmd.subs.iter().map(|sub| sub.name).collect();
        subs.push(super::HELP_CMD.0);
        let all = subs.len();
        subs.sort_unstable();
        subs.dedup();
        assert_eq!(subs.len(), all, "{at}: a subcommand name twice");
        let mut flags: Vec<_> = ROOT.flags.iter().map(|f| f.long).collect();
        if path.len() > 1 {
            flags.extend(cmd.flags.iter().map(|f| f.long));
        }
        flags.extend(["help", "version"]);
        let all = flags.len();
        flags.sort_unstable();
        flags.dedup();
        assert_eq!(flags.len(), all, "{at}: a flag name twice");
    }
}

#[test]
fn every_command_parses_to_its_op() {
    for path in commands() {
        let cmd = path[path.len() - 1];
        let Some(want) = cmd.op else { continue };
        let mut argv: Vec<_> = path[1..].iter().map(|c| c.name.to_owned()).collect();
        let required = cmd.args.iter().filter(|a| a.required);
        argv.extend(required.map(|a| a.name.to_lowercase()));
        assert_eq!(op(&argv.join(" ")), want, "{argv:?}");
    }
}

#[test]
fn flags_and_arguments_reach_their_command() {
    let m = parse("resume --any 0190 --host box").unwrap();
    assert_eq!(
        (m.op, m.arg(0), m.on(&ANY)),
        (Op::Resume, Some("0190"), true)
    );
    assert_eq!(m.value(&HOST), Some("box"));
    let m = parse("--host=box daemon stop").unwrap();
    assert_eq!(
        (m.op, m.value(&HOST)),
        (Op::Daemon(Daemon::Stop), Some("box"))
    );
    assert!(!parse("resume").unwrap().on(&ANY));
}

#[test]
fn the_daemon_parses_what_a_client_spawns_it_with() {
    let m = parse("__daemon --listen-fd 3 --lock-fd 4 --reset-breaker").unwrap();
    assert_eq!(m.op, Op::Serve);
    assert_eq!(
        (m.value(&LISTEN_FD), m.value(&LOCK_FD)),
        (Some("3"), Some("4"))
    );
    assert!(m.on(&RESET_BREAKER));
}

#[test]
fn a_flag_not_given_falls_back_to_its_variable() {
    let env = |key: &str| (key == "A_HOST").then(|| OsString::from("box"));
    let m = super::parse([OsString::from("ls")], env).unwrap();
    assert_eq!(m.value(&HOST), Some("box"));
    let m = super::parse(["--host", "other"].map(OsString::from), env).unwrap();
    assert_eq!(m.value(&HOST), Some("other"));
}

#[test]
fn a_wrong_command_line_is_a_usage_error() {
    assert!(usage_error("bogus").contains("unexpected argument 'bogus'"));
    assert!(usage_error("ls extra").contains("Usage: a ls\n"));
    assert!(usage_error("resume a b").contains("unexpected argument 'b'"));
    assert!(usage_error("attach").contains("<ID>"));
    assert!(usage_error("web --any").contains("'--any'"));
    assert!(usage_error("--host").contains("--host"));
    assert!(usage_error("ls -x").contains("'-x'"));
    // A group alone shows what it groups.
    assert!(usage_error("daemon").contains("restart"));
}

#[test]
fn help_lists_what_the_table_holds() {
    let root = help("--help");
    for sub in ROOT.subs {
        assert_eq!(root.contains(sub.name), !sub.hidden, "{}", sub.name);
    }
    assert!(root.contains("[env: A_HOST]") && root.contains("--version"));
    for argv in ["daemon --help", "help daemon", "daemon -h"] {
        let text = help(argv);
        assert!(text.contains("Usage: a daemon <COMMAND>"), "{argv}: {text}");
        assert!(
            text.contains("Global flags:\n      --host <HOST>"),
            "{argv}: {text}"
        );
    }
    assert!(help("help daemon stop").starts_with("Stop the daemon;"));
    assert_eq!(help("-V"), format!("a {}\n", super::VERSION));
}
