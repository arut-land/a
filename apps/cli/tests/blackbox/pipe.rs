//! `a` in a pipeline (docs/cli.md): stdin not a terminal, or `-p`, runs
//! one turn and exits with a status that says how it ended; stdout gets
//! what the agent says and nothing else.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Output, Stdio};

use crate::support::{TestProcess, deadline};

use crate::common::{Env, wait_for};
use rustix::process::{Pid, Signal, kill_process};

/// `a args` with `input` on stdin, run to its end.
fn piped(env: &Env, args: &[&str], input: &str) -> Output {
    TestProcess::from(spawn(env, args, input))
        .collect(deadline())
        .unwrap()
}

/// `a args` with `input` written to its stdin, which is then closed.
fn spawn(env: &Env, args: &[&str], input: &str) -> Child {
    let mut child = env
        .cmd(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_owned();
    // Written aside: a large input fills the pipe before `a` reads it.
    std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    child
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

#[test]
fn pipe_output_to_a_tty_cannot_inject_terminal_controls() {
    let env = Env::new("pipe-tty-controls");
    let mut term = env.tty(&["-p", "\x1b]0;injected-title\x07héllo 世界"], 80, 24);
    assert!(term.wait().success());
    term.wait_for("héllo 世界");
    let output = term.bytes();
    assert!(
        !output
            .windows(b"\x1b]0;injected-title\x07".len())
            .any(|w| w == b"\x1b]0;injected-title\x07"),
        "pipe mode wrote an OSC command to the terminal: {output:?}"
    );
}

/// The short id of the one session `a ls` lists.
fn only_session(env: &Env) -> String {
    let listed = env.run(&["ls"]);
    let lines: Vec<_> = listed.lines().collect();
    assert_eq!(lines.len(), 1, "{listed}");
    lines[0].split_whitespace().next().unwrap().to_owned()
}

#[test]
fn piped_stdin_is_one_prompt_and_only_the_reply_reaches_stdout() {
    let env = Env::new("pipe-reply");
    let out = piped(&env, &[], "two\nlines\n");
    assert!(out.status.success(), "{out:?}");
    // The echo agent's reply, and no session line, escape or spinner.
    assert_eq!(text(&out.stdout), "two\nlines\n");
    assert_eq!(text(&out.stderr), "");
    only_session(&env);
}

#[test]
fn a_prompt_goes_first_and_piped_stdin_follows_it() {
    let env = Env::new("pipe-prompt");
    let out = piped(&env, &["-p", "sum up"], "l1\nl2\n");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(text(&out.stdout), "sum up\n\nl1\nl2\n");
    let out = piped(&env, &["--prompt", "again", "resume"], "");
    assert_eq!(text(&out.stdout), "again\n");
    only_session(&env);
}

#[test]
fn nothing_to_send_is_a_usage_error() {
    let env = Env::new("pipe-empty");
    let out = piped(&env, &[], "\n  \n");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("nothing to send"),
        "{}",
        text(&out.stderr)
    );
    let out = piped(&env, &["-p", "hi", "ls"], "");
    assert_eq!(out.status.code(), Some(2));
}

/// No pipeline waits for another client to approve: the run ends, says
/// what waits where and how to go on, and the question stays open in the
/// session for a client to answer.
#[test]
fn a_call_needing_approval_ends_a_piped_run_and_waits_in_the_session() {
    let env = Env::new("pipe-approval");
    env.ext("policy", "ask = [\"bash\"]");
    let out = piped(&env, &[], "$ echo ran > ran.txt");
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    let id = only_session(&env);
    let told = text(&out.stderr);
    for said in [
        "bash ($ echo ran > ran.txt) waits for approval",
        "Why: the ask mode requires approval for this tool",
        &format!("session {id}"),
        &format!("`a attach {id}`"),
        "`a web`",
        "--mode auto",
        "A_MODE=auto",
    ] {
        assert!(told.contains(said), "{said:?} not in {told}");
    }
    let ran = env.ws.join("ran.txt");
    assert!(!ran.exists(), "ran unapproved");

    let mut other = env.repl(&["attach", &id]);
    other.lines_until("? $ echo ran > ran.txt");
    other.type_line("y");
    assert_eq!(other.lines_until("└"), ["  · approved", "  └ exit 0"]);
    other.finish();
    assert_eq!(fs::read_to_string(ran).unwrap(), "ran\n");
}

#[test]
fn the_mode_flag_or_its_variable_lets_a_piped_run_through() {
    let mut env = Env::new("pipe-mode");
    env.ext("policy", "ask = [\"bash\"]");
    let out = piped(&env, &["--mode", "auto"], "$ echo one > one.txt");
    assert!(out.status.success(), "{out:?}");
    assert!(env.ws.join("one.txt").exists());
    env.vars.push(("A_MODE", "auto"));
    let out = piped(&env, &[], "$ echo two > two.txt");
    assert!(out.status.success(), "{out:?}");
    assert!(env.ws.join("two.txt").exists());
    let out = piped(&env, &["--mode", "nope"], "hi");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("/mode nope"),
        "{}",
        text(&out.stderr)
    );
}

/// `a | head -1`: once the reader has gone, `a` ends as SIGPIPE ends a
/// program (status 141), with nothing on stderr, and the session goes on.
#[test]
fn a_reader_that_goes_ends_a_quietly_and_the_session_goes_on() {
    let env = Env::new("pipe-epipe");
    let input = "a line of the reply, longer than the pipe holds\n".repeat(20_000);
    let mut child = spawn(&env, &[], &input);
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert_eq!(first, "a line of the reply, longer than the pipe holds\n");
    let out = TestProcess::from(child).collect(deadline()).unwrap();
    assert_eq!(out.status.code(), Some(141), "{}", text(&out.stderr));
    assert_eq!(out.stderr, b"");
    let id = only_session(&env);
    let mut again = env.repl(&["attach", &id]);
    again.lines_until("[session");
    again.finish();
}

#[test]
fn ctrl_c_interrupts_a_piped_turn_and_exits_130() {
    let env = Env::new("pipe-sigint");
    let child = spawn(&env, &[], "$ sleep 30");
    wait_for("the command to run", || {
        env.run(&["ls"]).contains(" running ")
    });
    kill_process(Pid::from_child(&child), Signal::INT).unwrap();
    let out = TestProcess::from(child).collect(deadline()).unwrap();
    assert_eq!(out.status.code(), Some(130));
    wait_for("the turn to end", || env.run(&["ls"]).contains(" idle "));
}
