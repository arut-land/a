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
    let screen = term.wait_until("certified idle after upgrade", |screen| {
        screen.find("⏺ after the upgrade").is_some() && screen.find("message the agent").is_some()
    });
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
        let screen = term.wait_until("certified idle after continue", |screen| {
            screen.find("⏺ after continue").is_some() && screen.find("message the agent").is_some()
        });
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

#[test]
fn suspension_and_termination_restore_a_terminal_that_does_not_drain() {
    use rustix::process::{WaitOptions, waitpid};
    use std::sync::mpsc;
    use std::time::Duration;

    let env = Env::new("tui-suspend-unread");
    let mut term = env.tty(&[], 400, 250);
    term.wait_for("● live");
    term.send(&vec![b'x'; 100_000]);
    term.send(b"\r");
    term.wait_for(&"x".repeat(300));
    term.go_deaf_drawing(b"\x0c");
    let pid = Pid::from_child(&term.child);
    let (send, stopped) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(waitpid(Some(pid), WaitOptions::UNTRACED));
    });
    kill_process(pid, Signal::TSTP).unwrap();
    let (_, status) = stopped
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|error| panic!(
            "suspension waited for an unread terminal: {error}; cooked={}, parked={}, threads={:?}",
            term.cooked(), crate::common::threads_parked(term.child.id()),
            crate::common::thread_names(term.child.id()),
        ))
        .unwrap()
        .unwrap();
    assert!(status.stopped());
    assert!(term.cooked());
    kill_process(pid, Signal::CONT).unwrap();
    kill_process(pid, Signal::TERM).unwrap();
    assert_eq!(term.wait().signal(), Some(Signal::TERM.as_raw()));
    assert!(term.cooked());
}

#[test]
fn an_empty_title_reports_failure_and_preserves_the_previous_title() {
    rejected_title("empty");
}

#[test]
fn a_whitespace_title_reports_failure_and_preserves_the_previous_title() {
    rejected_title("whitespace");
}

#[test]
fn a_quote_only_title_reports_failure_and_preserves_the_previous_title() {
    rejected_title("quotes");
}

#[test]
fn title_generation_errors_are_visible_and_preserve_the_previous_title() {
    rejected_title("error");
}

fn rejected_title(reply: &str) {
    let env = Env::new("tui-title-failure");
    env.ext("retry", "attempts = 0\ntransport = 0");
    let mut term = env.tty(&[], 120, 40);
    term.wait_for("● live");
    term.send(b"/title fixture:title:Previous title\r");
    term.wait_for("⎿  Previous title");
    crate::common::wait_for("previous terminal title", || {
        term.bytes()
            .windows(b"\x1b]2;Previous title\x1b\\".len())
            .any(|w| w == b"\x1b]2;Previous title\x1b\\")
    });
    term.send(format!("/title fixture:title:{reply}\r").as_bytes());
    let screen = term.wait_for("Title was not changed:");
    assert!(
        screen.text().contains(if reply == "error" {
            "fixture title generation failed"
        } else {
            "the model returned an empty title"
        }),
        "{}",
        screen.text()
    );
    term.send(b"\x04");
    assert!(term.wait().success());
    let bytes = term.bytes();
    let titles: Vec<_> = bytes
        .split(|b| *b == 0x1b)
        .filter(|part| part.starts_with(b"]2;"))
        .collect();
    assert_eq!(
        titles,
        [b"]2;a".as_slice(), b"]2;Previous title".as_slice()]
    );
}

#[test]
fn a_title_without_a_provider_explains_why_it_was_not_changed() {
    unavailable_title(None);
}

#[test]
fn terminal_titles_follow_early_and_repeated_requests_and_restore_in_both_viewports() {
    for viewport in ["full", "inline"] {
        let mut env = Env::new("tui-title-lifecycle");
        env.vars.push(("A_TUI", viewport));
        let mut term = env.tty(&[], 120, 40);
        term.wait_for("● live");
        // No user turn has run yet.
        term.send(b"/title\r");
        term.wait_for("⎿  fixture title");
        term.send(b"/title fixture:title:Named conversation\r");
        term.wait_for("⎿  Named conversation");
        term.send(b"/title\r");
        crate::common::wait_for("regenerated title", || {
            String::from_utf8_lossy(&term.bytes())
                .matches("\x1b]2;fixture title\x1b\\")
                .count()
                == 2
        });
        term.send(b"\x04");
        assert!(term.wait().success());
        let bytes = term.bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(text.matches("\x1b[22;2t").count(), 1);
        assert_eq!(text.matches("\x1b[23;2t").count(), 1);
        let titles: Vec<_> = text
            .split('\x1b')
            .filter(|s| s.starts_with("]2;"))
            .collect();
        assert_eq!(
            titles,
            [
                "]2;a",
                "]2;fixture title",
                "]2;Named conversation",
                "]2;fixture title"
            ]
        );
        assert!(term.cooked());
    }
}

#[test]
fn a_title_requested_during_a_turn_eventually_changes_the_terminal_title() {
    let env = Env::new("tui-title-running");
    let mut term = env.tty(&[], 120, 40);
    term.wait_for("● live");
    term.send(b"sleep 1\r");
    term.wait_for("Working…");
    term.send(b"/title fixture:title:While running\r");
    term.wait_for("⎿  While running");
    term.send(b"\x04");
    assert!(term.wait().success());
    assert!(String::from_utf8_lossy(&term.bytes()).contains("\x1b]2;While running\x1b\\"));
}

#[test]
fn termination_restores_the_saved_terminal_title_in_both_viewports() {
    for viewport in ["full", "inline"] {
        let mut env = Env::new("tui-title-signal");
        env.vars.push(("A_TUI", viewport));
        let mut term = env.tty(&[], 100, 30);
        term.wait_for("● live");
        kill_process(Pid::from_child(&term.child), Signal::TERM).unwrap();
        assert_eq!(term.wait().signal(), Some(libc::SIGTERM));
        let bytes = term.bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(text.matches("\x1b[22;2t").count(), 1);
        assert_eq!(text.matches("\x1b[23;2t").count(), 1);
        assert!(term.cooked());
    }
}

#[test]
fn switching_the_viewed_conversation_changes_the_terminal_title() {
    for viewport in ["full", "inline"] {
        let mut env = Env::new("tui-title-switch");
        env.vars.push(("A_TUI", viewport));
        let mut term = env.tty(&[], 120, 40);
        term.wait_for("● live");
        term.send(b"/title fixture:title:Parent title\r");
        term.wait_for("⎿  Parent title");
        term.send(b"call background_task {\"prompt\":\"child greeting\",\"messages\":[]}\r");
        term.wait_for("child greeting");
        term.wait_until("child in the conversation switcher", |screen| {
            screen.text().contains("agent main") && screen.text().contains("echo child greeting")
        });
        term.send(b"\x1b[B\x1b[B\r");
        term.wait_for("Message @");
        term.send(b"/title fixture:title:Child title\r");
        term.wait_for("⎿  Child title");
        term.send(b"\x1b[B\r");
        crate::common::wait_for("parent terminal title after switching back", || {
            String::from_utf8_lossy(&term.bytes())
                .matches("\x1b]2;Parent title\x1b\\")
                .count()
                == 2
        });
        term.send(b"\x04");
        assert!(term.wait().success());
        let bytes = term.bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\x1b]2;Child title\x1b\\"));
        assert_eq!(text.matches("\x1b[22;2t").count(), 1);
        assert_eq!(text.matches("\x1b[23;2t").count(), 1);
    }
}

#[test]
fn a_title_without_a_resolvable_model_explains_why_it_was_not_changed() {
    unavailable_title(Some("openai:"));
}

fn unavailable_title(model: Option<&str>) {
    let env = Env::new("tui-title-unavailable");
    if let Some(model) = model {
        let config = env.home.config_file();
        fs::write(
            &config,
            format!(
                "fast_model = {model:?}\n{}",
                fs::read_to_string(&config).unwrap()
            ),
        )
        .unwrap();
    }
    env.ext("retry", "attempts = 0\ntransport = 0");
    // Bootstrap clears client variables; start this owned foreground daemon
    // explicitly so its fixture transport can be disabled.
    let mut daemon = title_command(&env, &["__daemon"])
        .env("A_FIXTURE_SIDE_PROVIDER", "off")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    crate::common::wait_for("fixture daemon", || env.pid().is_some());
    let mut term = Env::tty_of(title_command(&env, &[]), 120, 40);
    term.wait_for("● live");
    term.send(b"/title\r");
    term.wait_for("Title was not changed:");
    if model.is_some() {
        term.wait_for("names no model");
    }
    term.send(b"\x04");
    assert!(term.wait().success());
    env.run(&["daemon", "stop"]);
    assert!(daemon.wait().unwrap().success());
}

fn title_command(env: &Env, args: &[&str]) -> std::process::Command {
    let mut command = env.cmd(args);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", env.root.join("user"))
        .env("A_HOME", env.home.root())
        .env("OPENAI_API_KEY", "loopback-dummy")
        .env("OPENAI_BASE_URL", "http://127.0.0.1:1/v1")
        .env("NO_PROXY", "127.0.0.1,localhost");
    command
}
