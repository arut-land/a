//! The derive is the command line: these hold its spec to the shape the
//! client assumes, and argv to the commands it names.

use std::ffi::{OsStr, OsString};
use std::process::Command;

use usage_rs::embedded::{Exit, Outcome};

use super::generate::commands;
use super::{Args, Cmd, Daemon};

fn words(argv: &str) -> Vec<OsString> {
    argv.split_whitespace().map(OsString::from).collect()
}

fn outcome(argv: &str) -> Outcome<Args> {
    Args::embedded_outcome(&words(argv))
}

fn parse(argv: &str) -> Args {
    match outcome(argv) {
        Outcome::Parsed(parsed) => parsed,
        Outcome::Exit(exit) => panic!("{argv}: {exit:?}"),
    }
}

/// `text` without the escapes a terminal would colour it with.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        rest = tail.find('m').map_or("", |end| &tail[end + 1..]);
    }
    out + rest
}

fn exit(argv: &str) -> Exit {
    match outcome(argv) {
        Outcome::Exit(exit) => Exit {
            text: plain(&exit.text),
            ..exit
        },
        Outcome::Parsed(parsed) => panic!("{argv}: {parsed:?}"),
    }
}

fn usage_error(argv: &str) -> String {
    let exit = exit(argv);
    assert!(exit.stderr && exit.code == 2, "{argv}: {exit:?}");
    exit.text
}

fn help(argv: &str) -> String {
    let exit = exit(argv);
    assert!(!exit.stderr && exit.code == 0, "{argv}: {exit:?}");
    exit.text
}

#[test]
fn every_visible_command_says_what_it_does() {
    for path in commands() {
        let cmd = path[path.len() - 1];
        let hidden = path.iter().any(|c| c.hide());
        assert_eq!(hidden, cmd.cmd().name == "__daemon", "{}", cmd.cmd().name);
        assert!(
            hidden || cmd.about().is_some_and(|about| !about.is_empty()),
            "{}: no help",
            cmd.cmd().name
        );
    }
}

#[test]
fn every_command_parses() {
    for path in commands().into_iter().skip(1) {
        let cmd = path[path.len() - 1];
        if !cmd.subcommands().is_empty() {
            continue;
        }
        let mut argv: Vec<_> = path[1..].iter().map(|c| c.cmd().name.to_owned()).collect();
        let required = cmd.args().iter().filter(|a| a.required());
        argv.extend(required.map(|a| a.arg().name.to_lowercase()));
        parse(&argv.join(" "));
    }
}

#[test]
fn flags_and_arguments_reach_their_command() {
    let args = parse("resume --any 0190 --host box");
    let resume = Cmd::Resume {
        id: Some("0190".into()),
        any: true,
    };
    assert_eq!(
        (args.cmd, args.host.as_deref()),
        (Some(resume), Some("box"))
    );
    let args = parse("--host=box daemon stop");
    let stop = Cmd::Daemon {
        action: Daemon::Stop,
    };
    assert_eq!((args.cmd, args.host.as_deref()), (Some(stop), Some("box")));
    let resume = Cmd::Resume {
        id: None,
        any: false,
    };
    assert_eq!(parse("resume").cmd, Some(resume));
    assert_eq!(parse("").cmd, None);
}

#[test]
fn the_daemon_parses_what_a_client_spawns_it_with() {
    let serve = Cmd::Serve {
        listen_fd: Some(3),
        lock_fd: Some(4),
        reset_breaker: true,
    };
    let args = parse("__daemon --listen-fd 3 --lock-fd 4 --reset-breaker");
    assert_eq!(args.cmd, Some(serve));
}

/// The variable is the process's, so the test asks a child of this test
/// binary, run with it set.
#[test]
fn a_flag_not_given_falls_back_to_its_variable() {
    const NAME: &str = "args::tests::a_flag_not_given_falls_back_to_its_variable";
    if let Some(argv) = std::env::var_os("A_ARGS_PROBE") {
        let words = words(&argv.to_string_lossy());
        let refs: Vec<&OsStr> = words.iter().map(OsString::as_os_str).collect();
        let host = Args::parse_from(&refs).expect("a command line").host;
        println!("host={host:?}");
        return;
    }
    for (argv, want) in [("ls", "box"), ("--host other ls", "other")] {
        let exe = std::env::current_exe().expect("the test binary");
        let out = Command::new(exe)
            .args(["--exact", NAME, "--nocapture"])
            .env("A_ARGS_PROBE", argv)
            .env("A_HOST", "box")
            .output()
            .expect("the test binary runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("host={:?}", Some(want))),
            "{argv}: {stdout}"
        );
    }
}

#[test]
fn a_wrong_command_line_is_a_usage_error() {
    assert!(usage_error("bogus").contains("'bogus'"));
    assert!(usage_error("ls extra").contains("Usage: a ls\n"));
    assert!(usage_error("resume a b").contains("unexpected argument 'b'"));
    assert!(usage_error("attach").contains("<ID>"));
    assert!(usage_error("web --any").contains("'--any'"));
    assert!(usage_error("--host").contains("--host"));
    assert!(usage_error("ls -x").contains("'-x'"));
    assert!(usage_error("__daemon --listen-fd x").contains("'x'"));
    // A group alone shows what it groups.
    assert!(usage_error("daemon").contains("restart"));
}

#[test]
fn help_lists_what_the_spec_holds() {
    let root = help("--help");
    for sub in Args::spec().root.subcommands() {
        let name = sub.cmd().name;
        assert_eq!(root.contains(name), !sub.hide(), "{name}");
    }
    assert!(root.contains("[env: A_HOST]") && root.contains("--version"));
    for argv in ["daemon --help", "help daemon", "daemon -h"] {
        let text = help(argv);
        assert!(
            text.contains("Usage: a daemon <SUBCOMMAND>"),
            "{argv}: {text}"
        );
        assert!(
            text.contains("Global flags:\n      --host <HOST>"),
            "{argv}: {text}"
        );
    }
    assert!(help("help daemon stop").starts_with("Stop the daemon;"));
    assert_eq!(help("-V"), format!("a {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn completion_offers_the_visible_commands() {
    let request = words("__complete_word__ --shell fish --words a d");
    let answer = Args::completion_request(&request).expect("a completion request");
    assert!(
        answer.lines().any(|line| line.starts_with("daemon\t")),
        "{answer}"
    );
    assert!(!answer.contains("__daemon"), "{answer}");
    assert!(Args::completion_request(&words("ls")).is_none());
}
