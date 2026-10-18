//! The terminal client, black box: `a` on a pseudo-terminal against an echo
//! agent daemon in its own `A_HOME`.
#![cfg(unix)]

mod common;

use std::fs;

use common::Env;

#[test]
fn the_full_screen_client_shows_replies_and_answers_approvals_by_click() {
    let mut env = Env::new("tui-full");
    env.vars.push(("A_ASK", "run_command"));
    let mut term = env.tty(&[], 100, 30);
    // The first frame does not wait for the daemon.
    term.wait_for("F1 help");
    eprintln!("spawn to first frame: {:?}", term.first_frame());
    term.wait_for("● live");

    term.send(b"hello from a tty\r");
    let screen = term.wait_for("  hello from a tty");
    assert!(
        screen.find("› hello from a tty").is_some(),
        "{}",
        screen.text()
    );

    term.send(b"$ echo ran > ran.txt\r");
    let screen = term.wait_for("? approve $ echo ran > ran.txt");
    assert!(screen.find("? waiting for approval").is_some());
    let ran = env.ws.join("ran.txt");
    assert!(!ran.exists(), "ran before it was approved");
    // The approval bar's own button, on the row above the input.
    let (x, y) = screen.find("? approve").unwrap();
    let bar = screen.row(usize::from(y));
    let yes = bar.find("[ y  yes ]").unwrap();
    term.click((
        x + u16::try_from(bar[..yes].chars().count()).unwrap() + 2,
        y,
    ));
    term.wait_for("✓ exit 0");
    assert_eq!(fs::read_to_string(&ran).unwrap(), "ran\n");

    // Ctrl-C with an empty input detaches and gives the terminal back.
    term.send(b"\x03");
    assert!(term.wait().success());
    let out = String::from_utf8_lossy(&term.bytes()).into_owned();
    assert!(out.contains("\x1b[?1049l"), "left the alternate screen");
    assert!(out.contains("[detached; the session keeps running"));
}

#[test]
fn the_inline_client_prints_the_transcript_into_the_scrollback() {
    let mut env = Env::new("tui-inline");
    env.vars.push(("A_ASK", "run_command"));
    let mut term = env.tty(&[], 80, 24);
    term.wait_for("● live");
    term.send(b"$ echo first\r");
    term.wait_for("? approve $ echo first");
    term.send(b"y");
    term.wait_for("✓ exit 0");
    term.send(b"\x03");
    assert!(term.wait().success());

    env.vars.push(("A_TUI", "inline"));
    let mut term = env.tty(&["resume"], 80, 24);
    let screen = term.wait_for("recap · you: $ echo first");
    assert!(
        screen.find("› $ echo first").is_some(),
        "the replay is printed above the input:\n{}",
        screen.text()
    );
    let out = String::from_utf8_lossy(&term.bytes()).into_owned();
    assert!(
        !out.contains("\x1b[?1049h"),
        "inline stays on the main screen"
    );
    assert!(
        !out.contains("\x1b[?1000h"),
        "and leaves the mouse to the terminal"
    );
    term.send(b"\x04");
    assert!(term.wait().success());
}
