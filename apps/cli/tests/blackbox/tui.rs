//! The terminal client, black box: `a` on a pseudo-terminal against an echo
//! agent daemon in its own `A_HOME`.

use std::fs;
use std::os::unix::process::ExitStatusExt;

use crate::common::Env;
use rustix::process::{Pid, Signal, kill_process};

#[test]
fn the_full_screen_client_shows_replies_and_answers_approvals_by_click() {
    let env = Env::new("tui-full");
    env.ext("policy", "ask = [\"bash\"]");
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

/// The first frame does not wait for the daemon: with the socket held by
/// something that never answers the hello, the screen is still painted,
/// and nothing in it asked the terminal a question.
#[test]
fn first_frame_before_the_daemon_answers() {
    let env = Env::new("tui-first-frame");
    let held = env.home.bind().unwrap();
    let term = env.tty(&[], 80, 24);
    let screen = term.wait_for("◌ connecting");
    assert!(screen.find("F1 help").is_some(), "{}", screen.text());
    let first = term.first_frame().expect("no frame");
    eprintln!("spawn to first frame, daemon silent: {first:?}");
    assert!(!term.bytes().windows(3).any(|w| w == b"\x1b[6n"));
    drop(held);
}

#[test]
fn the_inline_client_prints_the_transcript_into_the_scrollback() {
    let mut env = Env::new("tui-inline");
    env.ext("policy", "ask = [\"bash\"]");
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

#[test]
fn sigterm_gives_the_terminal_back_before_it_ends_the_client() {
    let env = Env::new("tui-sigterm");
    let mut term = env.tty(&[], 80, 24);
    term.wait_for("● live");
    assert!(!term.cooked(), "the client runs the terminal raw");
    let pid = Pid::from_child(&term.child);
    kill_process(pid, Signal::TERM).unwrap();
    assert_eq!(term.wait().signal(), Some(Signal::TERM.as_raw()));
    assert!(term.cooked(), "raw mode outlived the client");
    let out = String::from_utf8_lossy(&term.bytes()).into_owned();
    assert!(out.contains("\x1b[?1049l"), "left the alternate screen");
}

/// A terminal nobody reads (its emulator hung, or gone with a copy of its
/// master left open) blocks the client's every write: SIGTERM ends it all
/// the same, the terminal's mode given back.
#[test]
fn sigterm_ends_a_client_whose_terminal_is_not_read() {
    let env = Env::new("tui-unread");
    let mut term = env.tty(&[], 400, 250);
    term.wait_for("● live");
    // A screen full of text: a whole frame of it is 100 KB, past what any
    // terminal buffers.
    term.send(&vec![b'x'; 100_000]);
    term.send(b"\r");
    term.wait_for(&"x".repeat(300));
    term.go_deaf_drawing(b"\x0c");
    let pid = Pid::from_child(&term.child);
    kill_process(pid, Signal::TERM).unwrap();
    assert_eq!(term.wait().signal(), Some(Signal::TERM.as_raw()));
    assert!(term.cooked(), "raw mode outlived the client");
}

/// A daemon upgrade under the terminal client: it reattaches to the new
/// daemon, sends what was typed meanwhile, and never reads the drain as a
/// lost daemon.
#[test]
fn the_full_screen_client_rides_out_a_daemon_upgrade() {
    let env = Env::new("tui-drain");
    let old = env.older_build();
    let mut term = Env::tty_of(env.cmd_as(&old, &[]), 100, 30);
    term.wait_for("● live");
    term.send(b"before\r");
    term.wait_for("  before");
    let first = env.pid().unwrap();

    // The newer build drains the older one's daemon and starts its own.
    env.run(&["ls"]);
    assert_ne!(env.pid().unwrap(), first);
    term.send(b"after the upgrade\r");
    let screen = term.wait_for("  after the upgrade");
    assert!(!screen.text().contains("disconnected"), "{}", screen.text());
    assert_eq!(screen.text().matches("› after the upgrade").count(), 1);

    term.send(b"\x03");
    assert!(term.wait().success());
    let out = String::from_utf8_lossy(&term.bytes()).into_owned();
    assert!(out.contains("[detached; the session keeps running"));
}
