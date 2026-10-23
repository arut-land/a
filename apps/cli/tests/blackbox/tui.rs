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
    let screen = term.wait_for("⏺ hello from a tty");
    assert!(
        screen.find("❯ hello from a tty").is_some(),
        "{}",
        screen.text()
    );

    term.send(b"$ echo ran > ran.txt\r");
    let screen = term.wait_until("approval controls ready to click", |screen| {
        screen
            .find("Do you want to proceed? $ echo ran > ran.txt")
            .is_some()
            && screen.find("? waiting for approval").is_some()
            && screen.find("1. Yes").is_some()
    });
    let ran = env.ws.join("ran.txt");
    assert!(!ran.exists(), "ran before it was approved");
    // The approval bar's own choice, on the row under its question.
    let (x, y) = screen.find("1. Yes").unwrap();
    term.click((x + 2, y));
    // The idle hint follows ordered Activity; tool success alone is an entry prefix.
    term.wait_until("certified idle after tool completion", |screen| {
        screen.find("✓ exit 0").is_some() && screen.find("message the agent").is_some()
    });
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
    term.wait_for("Do you want to proceed? $ echo first");
    term.send(b"y");
    // The idle hint follows ordered Activity; tool success alone is an entry prefix.
    term.wait_until("certified idle after tool completion", |screen| {
        screen.find("✓ exit 0").is_some() && screen.find("message the agent").is_some()
    });
    term.send(b"\x03");
    assert!(term.wait().success());

    env.vars.push(("A_TUI", "inline"));
    let mut term = env.tty(&["resume"], 80, 24);
    let screen = term.wait_for("recap · you: $ echo first");
    assert!(
        screen.find("❯ $ echo first").is_some()
            || screen
                .scrollback
                .iter()
                .any(|line| line.contains("❯ $ echo first")),
        "the replay is printed above the input:\n{:?}\n{}",
        screen.scrollback,
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
    term.wait_for("⏺ before");
    let first = env.pid().unwrap();

    // The newer build drains the older one's daemon and starts its own.
    env.run(&["ls"]);
    assert_ne!(env.pid().unwrap(), first);
    term.send(b"after the upgrade\r");
    let screen = term.wait_for("⏺ after the upgrade");
    assert!(!screen.text().contains("disconnected"), "{}", screen.text());
    assert_eq!(screen.text().matches("❯ after the upgrade").count(), 1);

    term.send(b"\x03");
    assert!(term.wait().success());
    let out = String::from_utf8_lossy(&term.bytes()).into_owned();
    assert!(out.contains("[detached; the session keeps running"));
}

#[test]
fn esc_interrupts_a_running_turn_without_leaving_either_viewport() {
    for viewport in ["full", "inline"] {
        let mut env = Env::new("tui-esc");
        env.vars.push(("A_TUI", viewport));
        let mut term = env.tty(&[], 80, 24);
        term.wait_for("● live");
        term.send(b"$ echo started; sleep 60\r");
        term.wait_for("⎿  started");
        term.wait_for("Working…");
        term.send(b"\x1b");
        term.wait_until("interrupted and idle", |screen| {
            let text = screen.text();
            text.contains("aborted") && !text.contains("Working…")
        });
        term.send(b"still here\r");
        term.wait_for("⏺ still here");
        term.send(b"\x04");
        assert!(term.wait().success());
        assert!(term.cooked(), "detaching restores terminal input");
    }
}

#[test]
fn the_terminal_is_cooked_while_suspended_and_redrawn_after_continue() {
    use rustix::process::{WaitOptions, waitpid};
    use std::sync::mpsc;
    use std::time::Duration;

    for (viewport, external) in [
        ("full", false),
        ("full", true),
        ("inline", false),
        ("inline", true),
    ] {
        let mut env = Env::new("tui-job-control");
        env.vars.push(("A_TUI", viewport));
        let mut term = env.tty(&[], 80, 24);
        term.wait_for("● live");
        let pid = Pid::from_child(&term.child);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(waitpid(Some(pid), WaitOptions::UNTRACED));
        });
        if external {
            kill_process(pid, Signal::TSTP).unwrap();
        } else {
            term.send(b"\x1a");
        }
        let (_, stopped) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the client did not suspend")
            .unwrap()
            .unwrap();
        assert!(stopped.stopped());
        assert!(term.cooked(), "raw input outlived terminal ownership");
        term.shell_output(b"\r\nshell output while stopped\r\n");
        kill_process(pid, Signal::CONT).unwrap();
        term.wait_until("raw input after continue", |_| !term.cooked());
        assert!(!term.cooked(), "continuing did not reacquire raw input");
        term.send(b"after continue\r");
        let screen = term.wait_for("⏺ after continue");
        if viewport == "inline" {
            assert!(
                screen.find("shell output while stopped").is_some()
                    || screen
                        .scrollback
                        .iter()
                        .any(|line| line.contains("shell output while stopped")),
                "resuming overwrote shell output: {}",
                screen.text()
            );
        }
        term.send(b"\x03");
        assert!(term.wait().success());
        assert!(term.cooked());
    }
}
