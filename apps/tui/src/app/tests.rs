use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use protocol::{
    Activity, Body, Card, ClientMsg, DaemonMsg, Entry, Outcome, Residency, SessionId,
    SessionSummary, Target,
};
use serde_json::json;

use client::{Cmd, Event, EventKind, Io, Welcome};
use protocol::{Audience, CallId, Caps, Epoch, Json, Seq};

use super::{App, Effect};
use crate::keys::{Input, Key, Mouse};
use crate::render::{Grid, Viewport};
use crate::screen::Screen;
use crate::text::{Line, Modifier};

fn ws() -> PathBuf {
    PathBuf::from("/work/project")
}

fn entry(seq: u64, body: Body) -> DaemonMsg {
    DaemonMsg::Entry(Entry::new(Seq::new(seq).unwrap(), Epoch(0), body))
}

/// An app attached to a fresh session, with `msgs` folded in.
fn app(viewport: Viewport, msgs: &[DaemonMsg]) -> (App, SessionId) {
    let session = SessionId::new();
    let mut app = App::new(viewport, &Target::New);
    app.in_workspace(ws());
    connect(&mut app, false);
    app.fold(&DaemonMsg::Attached {
        session,
        workspace: ws(),
    });
    for msg in msgs {
        app.fold(msg);
    }
    (app, session)
}

/// Opens a connection for `app` (to a `remote` host's daemon).
fn connect(app: &mut App, remote: bool) {
    let opened = app.client.open(Target::New);
    client::pump(|_| Ok(()), app, opened);
    let conn = app.client.connection();
    let connected = app.client.step(Event {
        conn,
        kind: EventKind::Connected {
            welcome: Welcome {
                proto: 1,
                caps: Caps::ALL,
            },
            remote,
            commands: Vec::new(),
        },
    });
    client::pump(|_| Ok(()), app, connected);
}

/// A frame: its cells, where the caret shows (if it does), and its rows.
struct Drawn {
    grid: Grid,
    caret: Option<(u16, u16)>,
}

/// Draws a frame, and checks it is what drawing it on a blank canvas gives:
/// the rows kept from the frame before are the ones it would draw.
fn frame(app: &mut App, w: u16, h: u16) -> (Drawn, Vec<String>) {
    let caret = app.draw(w, h);
    let grid = app.canvas().grid().clone();
    let kept = std::mem::take(&mut app.canvas);
    assert_eq!(app.draw(w, h), caret);
    assert_eq!(app.canvas().grid(), &grid, "kept rows are stale");
    app.canvas = kept;
    let rows = grid.rows();
    (Drawn { grid, caret }, rows)
}

fn find(rows: &[String], text: &str) -> (u16, u16) {
    rows.iter()
        .enumerate()
        .find_map(|(y, row)| {
            let x = row.find(text)?;
            Some((
                u16::try_from(row[..x].chars().count()).unwrap(),
                u16::try_from(y).unwrap(),
            ))
        })
        .unwrap_or_else(|| panic!("{text:?} not on screen:\n{}", rows.join("\n")))
}

fn press(app: &mut App, key: Key) -> Option<Effect> {
    app.input(Input::Key(key))
}

fn typed(app: &mut App, text: &str) -> Option<Effect> {
    for c in text.chars() {
        press(app, Key::Char(c));
    }
    press(app, Key::Enter)
}

/// Types `text` without sending it.
fn keys(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, Key::Char(c));
    }
}

fn asked_command() -> Vec<DaemonMsg> {
    vec![
        entry(
            2,
            Body::Input {
                text: "$ ls".into(),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: None,
            },
        ),
        entry(
            3,
            Body::ToolCall {
                tool: "bash".into(),
                input: Json::encode(&json!({"command": "ls"})).unwrap(),
                id: None,
                card: Some(Card {
                    title: "$ ls".into(),
                    ..Card::default()
                }),
                by: Audience::Model,
            },
        ),
        entry(
            4,
            Body::Decision {
                by: "policy".into(),
                call: CallId::new(3),
                d: protocol::Verdict::Permission {
                    permission: protocol::Permission::Ask,
                },
            },
        ),
        DaemonMsg::Synced { seq: 4 },
    ]
}

#[test]
fn an_approval_shows_buttons_and_y_or_a_click_answers_it() {
    let (mut app, _) = app(Viewport::Full, &asked_command());
    let (_, rows) = frame(&mut app, 70, 20);
    assert!(rows.iter().any(|r| r == "❯ $ ls"), "{rows:#?}");
    find(&rows, "⏺ $ ls  ? waiting for approval");
    let bar = find(&rows, "Do you want to proceed? $ ls");
    assert_eq!(bar.1 + 3, find(&rows, &"─".repeat(70)).1);
    assert!(matches!(
        press(&mut app, Key::Char('y')),
        Some(Effect::Cmd(Cmd::Approve { call, approved: true })) if call.get() == 3
    ));
    let (x, y) = find(&rows, "2. No");
    assert!(matches!(
        app.input(Input::Mouse(Mouse::Press { x: x + 2, y })),
        Some(Effect::Cmd(Cmd::Approve { call, approved: false })) if call.get() == 3
    ));
    // With text in the input, y is typed.
    press(&mut app, Key::Char('x'));
    assert!(press(&mut app, Key::Char('y')).is_none());
}

/// An interrupt settles a call waiting for approval, so Ctrl-C sends one
/// rather than leaving.
#[test]
fn ctrl_c_interrupts_a_call_waiting_for_approval() {
    let (mut app, _) = app(Viewport::Full, &asked_command());
    assert!(app.client.turn().running);
    assert!(matches!(
        press(&mut app, Key::Ctrl('c')),
        Some(Effect::Cmd(Cmd::Interrupt))
    ));
}

#[test]
fn a_streamed_reply_shows_live_then_its_committed_text() {
    let (mut app, _) = app(
        Viewport::Full,
        &[
            entry(
                2,
                Body::Input {
                    text: "hi".into(),
                    mode: protocol::Mode::Queue,
                    taken: true,
                    origin: None,
                },
            ),
            entry(
                3,
                Body::ModelCall {
                    model: "gpt".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: None,
                    purpose: None,
                },
            ),
            DaemonMsg::Synced { seq: 3 },
            DaemonMsg::Delta {
                call: CallId::new(3).unwrap(),
                text: "Hel".into(),
                attempt: 0,
            },
            DaemonMsg::Delta {
                call: CallId::new(3).unwrap(),
                text: "lo **you**".into(),
                attempt: 0,
            },
        ],
    );
    let (screen, rows) = frame(&mut app, 50, 12);
    let (x, y) = find(&rows, "Hello you▍");
    assert!(
        screen.grid[(x + 6, y)].style.mods.contains(Modifier::BOLD),
        "markdown bold"
    );
    find(&rows, "● live");
    app.fold(&entry(
        4,
        Body::ModelResult {
            call: CallId::new(3).unwrap(),
            outcome: Outcome::Done {
                output: Json::encode(
                    &json!({"message": {"content": [{"type": "text", "text": "Hello you!"}]},
                               "usage": {"input": 12, "output": 3}}),
                )
                .unwrap(),
            },
        },
    ));
    let (_, rows) = frame(&mut app, 50, 12);
    find(&rows, "⏺ Hello you!");
    assert!(!rows.iter().any(|r| r.contains('▍')));
    find(&rows, "gpt  ↑12 ↓3");
}

#[test]
fn a_titling_reply_titles_the_session_and_is_no_reply_of_its_own() {
    let (mut app, _) = app(
        Viewport::Full,
        &[
            entry(
                3,
                Body::ModelCall {
                    model: "gpt".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: None,
                    purpose: Some("title".into()),
                },
            ),
            entry(
                4,
                Body::ModelResult {
                    call: CallId::new(3).unwrap(),
                    outcome: Outcome::Done {
                        output: Json::encode(
                            &json!({"message": {"content": [{"type": "text", "text": "\"Fix the parser\""}]},
                                   "usage": {"input": 1, "output": 1}}),
                        )
                        .unwrap(),
                    },
                },
            ),
            DaemonMsg::Synced { seq: 4 },
        ],
    );
    let (_, rows) = frame(&mut app, 60, 12);
    let (_, y) = find(&rows, "● live");
    assert!(rows[usize::from(y)].contains("Fix the parser"), "{rows:#?}");
    assert_eq!(
        rows.iter().filter(|r| r.contains("Fix the parser")).count(),
        1
    );
}

#[test]
fn a_card_folds_on_click_and_ctrl_o_shows_all_output() {
    let stdout = (1..=30).fold(String::new(), |s, n| s + &format!("out {n}\n"));
    let mut msgs = asked_command();
    msgs.push(entry(
        5,
        Body::ToolResult {
            call: CallId::new(3).unwrap(),
            outcome: Outcome::Done {
                output: Json::encode(
                    &json!({"output": stdout, "exit": 0, "cwd": "/w", "duration_ms": 1}),
                )
                .unwrap(),
            },
            card: Some(Card {
                title: "$ ls".into(),
                body: stdout.clone(),
                note: "exit 0".into(),
                ..Card::default()
            }),
        },
    ));
    let (mut app, _) = app(Viewport::Full, &msgs);
    let (_, rows) = frame(&mut app, 60, 40);
    find(&rows, "… +20 lines");
    let (x, y) = find(&rows, "⏺ $ ls  ✓ exit 0");
    assert!(app.input(Input::Mouse(Mouse::Press { x, y })).is_none());
    let (_, rows) = frame(&mut app, 60, 40);
    find(&rows, "⏺ $ ls  ✓ exit 0");
    find(&rows, "⎿  30 lines");
    let (x, y) = find(&rows, "⏺ $ ls");
    app.input(Input::Mouse(Mouse::Press { x, y }));
    press(&mut app, Key::Ctrl('o'));
    let (_, rows) = frame(&mut app, 60, 40);
    find(&rows, "out 1");
}

#[test]
fn the_wheel_scrolls_and_new_rows_keep_a_scrolled_view_still() {
    let msgs: Vec<_> = (2..40)
        .map(|seq| {
            entry(
                seq,
                Body::Input {
                    text: format!("message {seq}"),
                    mode: protocol::Mode::Queue,
                    taken: true,
                    origin: None,
                },
            )
        })
        .collect();
    let (mut app, _) = app(Viewport::Full, &msgs);
    let (_, rows) = frame(&mut app, 40, 12);
    find(&rows, "message 39");
    app.input(Input::Mouse(Mouse::WheelUp));
    let (_, rows) = frame(&mut app, 40, 12);
    assert!(!rows.iter().any(|r| r.contains("message 39")));
    let top = rows[0].clone();
    find(&rows, "Jump to bottom");
    app.fold(&entry(
        40,
        Body::Input {
            text: "message 40".into(),
            mode: protocol::Mode::Queue,
            taken: true,
            origin: None,
        },
    ));
    let (_, rows) = frame(&mut app, 40, 12);
    assert_eq!(rows[0], top, "the view did not move");
    press(&mut app, Key::Esc);
    let (_, rows) = frame(&mut app, 40, 12);
    find(&rows, "message 40");
}

#[test]
fn hints_stay_below_the_input_and_leave_the_caret_clear() {
    let (mut app, _) = app(Viewport::Full, &[]);
    let (screen, rows) = frame(&mut app, 60, 10);
    let (x, y) = find(&rows, "message the agent");
    assert_eq!(x, 0);
    assert_eq!(screen.caret, Some((2, y - 2)));
}

#[test]
fn ctrl_c_clears_the_input_then_detaches() {
    let (mut app, _) = app(Viewport::Full, &[]);
    press(&mut app, Key::Char('a'));
    assert!(press(&mut app, Key::Ctrl('c')).is_none());
    assert!(app.editor.is_empty());
    assert!(matches!(
        press(&mut app, Key::Ctrl('c')),
        Some(Effect::Detach)
    ));
    let mut unattached = App::new(Viewport::Full, &Target::New);
    assert!(matches!(
        press(&mut unattached, Key::Ctrl('c')),
        Some(Effect::Quit)
    ));
}

#[test]
fn ctrl_c_interrupts_a_running_call_and_shows_how_it_ended() {
    let mut msgs = asked_command();
    msgs.push(entry(
        5,
        Body::Answer {
            call: CallId::new(3).unwrap(),
            value: Json::encode(&true).unwrap(),
            origin: None,
        },
    ));
    let (mut app, _) = app(Viewport::Full, &msgs);
    press(&mut app, Key::Char('x'));
    assert!(matches!(
        press(&mut app, Key::Ctrl('c')),
        Some(Effect::Cmd(Cmd::Interrupt))
    ));
    assert!(!app.editor.is_empty(), "the draft stays");
    app.fold(&entry(
        6,
        Body::ToolResult {
            call: CallId::new(3).unwrap(),
            outcome: Outcome::Done {
                output: Json::encode(
                    &json!({"output": "", "exit": null, "signal": "SIGTERM", "aborted": true}),
                )
                .unwrap(),
            },
            card: Some(Card {
                title: "$ ls".into(),
                body: String::new(),
                note: "aborted · killed by SIGTERM".into(),
                mark: protocol::Mark::Bad,
            }),
        },
    ));
    let (_, rows) = frame(&mut app, 60, 12);
    find(&rows, "⏺ $ ls  ✗ aborted · killed by SIGTERM");
    assert!(
        press(&mut app, Key::Ctrl('c')).is_none(),
        "clears the input"
    );
    assert!(matches!(
        press(&mut app, Key::Ctrl('c')),
        Some(Effect::Detach)
    ));
}

#[test]
fn ctrl_c_interrupts_a_running_model_call_too() {
    let msgs = [
        entry(
            1,
            Body::Input {
                text: "hi".into(),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: None,
            },
        ),
        entry(
            2,
            Body::ModelCall {
                model: "m".into(),
                attempt: 0,
                choice: protocol::ToolChoice::Auto,
                r#for: None,
                purpose: None,
            },
        ),
    ];
    let (mut app, _) = app(Viewport::Full, &msgs);
    assert!(matches!(
        press(&mut app, Key::Ctrl('c')),
        Some(Effect::Cmd(Cmd::Interrupt))
    ));
    app.fold(&entry(
        3,
        Body::ModelResult {
            call: CallId::new(2).unwrap(),
            outcome: Outcome::Interrupted {
                output: Some("so far".into()),
            },
        },
    ));
    assert!(!app.client.turn().running);
}

#[test]
fn slash_commands() {
    let (mut app, current) = app(Viewport::Full, &[]);
    assert!(matches!(typed(&mut app, "/detach"), Some(Effect::Detach)));
    assert!(matches!(
        typed(&mut app, "/env refresh"),
        Some(Effect::Cmd(Cmd::RefreshEnv))
    ));
    assert!(matches!(
        typed(&mut app, "/new"),
        Some(Effect::Cmd(Cmd::Open(Target::New)))
    ));
    // The daemon runs the rest, or says there is no such command.
    assert!(matches!(
        typed(&mut app, "/nope"),
        Some(Effect::Cmd(Cmd::Input(text))) if text == "/nope"
    ));
    assert!(matches!(
        typed(&mut app, "/interrupt"),
        Some(Effect::Cmd(Cmd::Interrupt))
    ));
    assert!(matches!(
        typed(&mut app, "/steer and tests"),
        Some(Effect::Cmd(Cmd::Steer(text))) if text == "and tests"
    ));
    assert!(matches!(
        typed(&mut app, "hello there"),
        Some(Effect::Cmd(Cmd::Input(text))) if text == "hello there"
    ));
    // A remote host's environment is not this shell's to send.
    connect(&mut app, true);
    let refused = app.command(Cmd::RefreshEnv);
    client::pump(|io| panic!("{io:?}"), &mut app, refused);
    let (_, rows) = frame(&mut app, 80, 14);
    find(&rows, "reattach to refresh it");
    connect(&mut app, false);
    app.fold(&DaemonMsg::Attached {
        session: current,
        workspace: ws(),
    });

    assert!(matches!(
        typed(&mut app, "/sessions"),
        Some(Effect::Cmd(Cmd::Sessions))
    ));
    let other = SessionId::new();
    let summary = |id, workspace: &str, activity| SessionSummary {
        id,
        workspace: workspace.into(),
        entries: 3,
        clients: 0,
        activity,
        residency: Residency::Loaded,
        read_only: None,
    };
    // As the daemon lists them: newest first, which the picker keeps.
    app.fold(&DaemonMsg::Sessions {
        sessions: vec![
            summary(current, "/work/project", Activity::Approval),
            summary(other, "/elsewhere", Activity::Idle),
        ],
    });
    let (_, rows) = frame(&mut app, 80, 14);
    let (_, here) = find(&rows, &format!("• {}  approval   loaded", current.short()));
    let (x, there) = find(&rows, &format!("{}  idle", other.short()));
    assert!(here < there, "newest first");
    assert!(matches!(
        app.input(Input::Mouse(Mouse::Press { x, y: there })),
        Some(Effect::Cmd(Cmd::Open(Target::Id(id)))) if id == other
    ));
}

#[test]
fn resuming_where_there_is_no_session_opens_the_picker() {
    let mut app = App::new(
        Viewport::Full,
        &Target::Latest {
            any_workspace: false,
        },
    );
    app.in_workspace(ws());
    connect(&mut app, false);
    let effects = app.fold(&DaemonMsg::Error {
        message: "no matching session".into(),
    });
    assert!(
        matches!(&effects[..], [Io::Send(ClientMsg::List)]),
        "{effects:?}"
    );
    let (_, rows) = frame(&mut app, 60, 12);
    find(&rows, "sessions");
    assert!(matches!(
        press(&mut app, Key::Char('n')),
        Some(Effect::Cmd(Cmd::Open(Target::New)))
    ));
}

#[test]
fn the_recap_shows_until_dismissed() {
    let (mut app, _) = app(Viewport::Full, &asked_command());
    let (_, rows) = frame(&mut app, 80, 10);
    let (x, y) = find(&rows, "×");
    find(
        &rows,
        "↺ recap · you: $ ls · ran bash · 1 waiting for approval",
    );
    app.input(Input::Mouse(Mouse::Press { x, y }));
    let (_, rows) = frame(&mut app, 80, 10);
    assert!(!rows.iter().any(|r| r.contains("recap")));
}

#[test]
fn inline_prints_settled_items_once_and_keeps_the_rest_live() {
    let (mut app, _) = app(Viewport::Inline, &asked_command());
    let printed: Vec<String> = app.scrollback(60).iter().map(Line::text).collect();
    assert!(printed.contains(&"❯ $ ls".to_owned()), "{printed:?}");
    assert!(app.scrollback(60).is_empty(), "printed once");
    let height = app.height(60, 40);
    let (_, rows) = frame(&mut app, 60, height);
    find(&rows, "? waiting for approval");
    assert!(!rows.iter().any(|r| r == "❯ $ ls"), "{rows:#?}");
    app.fold(&entry(
        5,
        Body::ToolResult {
            call: CallId::new(3).unwrap(),
            outcome: Outcome::Done {
                output: Json::encode(&json!({"output": "a\n", "exit": 0})).unwrap(),
            },
            card: Some(Card {
                title: "$ ls".into(),
                body: "a\n".into(),
                note: "exit 0".into(),
                ..Card::default()
            }),
        },
    ));
    let printed: Vec<String> = app.scrollback(60).iter().map(Line::text).collect();
    assert!(
        printed.iter().any(|l| l.contains("✓ exit 0")),
        "{printed:?}"
    );
}

#[test]
fn draws_at_tiny_sizes_without_panicking() {
    let (mut app, _) = app(Viewport::Full, &asked_command());
    for (w, h) in [(1, 1), (10, 4), (3, 30)] {
        frame(&mut app, w, h);
    }
    press(&mut app, Key::F(1));
    frame(&mut app, 10, 4);
}

/// A terminal and what it was sent: `write(2)` calls and bytes.
#[derive(Clone, Default)]
struct Term(std::rc::Rc<std::cell::RefCell<(usize, Vec<u8>)>>);

impl Term {
    /// The writes since last asked, and their bytes.
    fn take(&self) -> (usize, Vec<u8>) {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

impl std::io::Write for Term {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut out = self.0.borrow_mut();
        out.0 += 1;
        out.1.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The frames a session goes through on `viewport`, drawn on `screen`:
/// each step's writes and bytes.
fn session(viewport: Viewport) -> Vec<(&'static str, usize, Vec<u8>)> {
    let (mut app, _) = app(viewport, &asked_command());
    let term = Term::default();
    let mut screen = Screen::new(viewport, term.clone());
    let mut out = Vec::new();
    let mut step = |what, app: &mut App, screen: &mut Screen<Term>, size| {
        screen.draw(app, size).unwrap();
        let (writes, bytes) = term.take();
        out.push((what, writes, bytes));
    };
    step("first", &mut app, &mut screen, (80, 24));
    step("idle", &mut app, &mut screen, (80, 24));
    app.fold(&DaemonMsg::Ack { client_seq: 1 });
    step("ack", &mut app, &mut screen, (80, 24));
    app.fold(&DaemonMsg::Sessions {
        sessions: Vec::new(),
    });
    step("sessions, no picker", &mut app, &mut screen, (80, 24));
    app.fold(&entry(
        5,
        Body::ToolResult {
            call: CallId::new(3).unwrap(),
            outcome: Outcome::Done {
                output: Json::encode(&json!({"output": "a\n", "exit": 0})).unwrap(),
            },
            card: Some(Card {
                title: "$ ls".into(),
                body: "a\n".into(),
                note: "exit 0".into(),
                ..Card::default()
            }),
        },
    ));
    step("result", &mut app, &mut screen, (80, 24));
    press(&mut app, Key::Char('x'));
    step("key", &mut app, &mut screen, (80, 24));
    press(&mut app, Key::F(1));
    step("overlay", &mut app, &mut screen, (80, 24));
    press(&mut app, Key::Esc);
    screen.invalidate();
    step("ctrl-l", &mut app, &mut screen, (80, 24));
    step("resize", &mut app, &mut screen, (60, 20));
    step("idle again", &mut app, &mut screen, (60, 20));
    screen.close().unwrap();
    let (writes, bytes) = term.take();
    out.push(("close", writes, bytes));
    out
}

#[test]
fn one_write_per_frame() {
    for viewport in [Viewport::Full, Viewport::Inline] {
        for (what, writes, bytes) in session(viewport) {
            assert!(writes <= 1, "{viewport:?} {what}: {writes} writes");
            if writes == 1 && what != "close" {
                assert!(
                    bytes.starts_with(b"\x1b[?2026h") && bytes.ends_with(b"\x1b[?2026l"),
                    "{viewport:?} {what}: not one synchronized update: {bytes:?}"
                );
            }
        }
    }
}

/// The event loop draws a frame only when the app says one is owed: an
/// ack, or a list no picker shows, costs no frame; a delta, a key or the
/// link changing owes one, which drawing pays.
#[test]
fn only_what_shows_owes_a_frame() {
    let (mut app, _) = app(Viewport::Full, &asked_command());
    assert!(app.needs_frame());
    app.draw(80, 24);
    assert!(!app.needs_frame());
    app.fold(&DaemonMsg::Ack { client_seq: 1 });
    app.fold(&DaemonMsg::Sessions {
        sessions: Vec::new(),
    });
    assert!(!app.needs_frame());
    app.fold(&DaemonMsg::Delta {
        call: CallId::new(2).unwrap(),
        text: "hi".into(),
        attempt: 0,
    });
    assert!(app.needs_frame());
    app.draw(80, 24);
    press(&mut app, Key::Char('x'));
    assert!(app.needs_frame());
    app.draw(80, 24);
    typed(&mut app, "/sessions");
    assert!(app.needs_frame());
}

/// A frame that shows nothing new is not written: no envelope, no
/// `write(2)`, so an ack or a list nothing shows costs no wake of the
/// terminal.
#[test]
fn an_idle_frame_writes_nothing() {
    for viewport in [Viewport::Full, Viewport::Inline] {
        for (what, writes, bytes) in session(viewport) {
            if what == "close" {
                continue;
            }
            let idle = matches!(what, "idle" | "ack" | "sessions, no picker" | "idle again");
            assert_eq!(
                idle,
                writes == 0 && bytes.is_empty(),
                "{viewport:?} {what}: {writes} writes, {bytes:?}"
            );
        }
    }
}

/// The client writes cursor moves, erases, SGR and modes, and nothing the
/// terminal would answer (no cursor position, attribute or color query):
/// a frame never waits on the terminal.
#[test]
fn no_terminal_queries() {
    use crate::term::{ENTER, ENTER_FULL, LEAVE, LEAVE_FULL};
    let mut all = vec![
        ENTER.to_vec(),
        ENTER_FULL.to_vec(),
        LEAVE.to_vec(),
        LEAVE_FULL.to_vec(),
    ];
    for viewport in [Viewport::Full, Viewport::Inline] {
        all.extend(session(viewport).into_iter().map(|(_, _, bytes)| bytes));
    }
    for bytes in all {
        let text = String::from_utf8(bytes).unwrap();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                assert!(
                    !c.is_control() || matches!(c, '\r' | '\n'),
                    "{c:?} in {text:?}"
                );
                continue;
            }
            assert_eq!(chars.next(), Some('['), "not a CSI in {text:?}");
            let fin = chars
                .by_ref()
                .find(|c| ('\x40'..='\x7e').contains(c))
                .unwrap();
            assert!(
                matches!(fin, 'A' | 'B' | 'G' | 'H' | 'J' | 'K' | 'h' | 'l' | 'm'),
                "CSI {fin} in {text:?}"
            );
        }
    }
}

/// Render cost on a long transcript, measured rather than checked:
/// `cargo test --release -p tui -- --ignored --nocapture render_cost`.
#[test]
#[ignore = "a measurement"]
#[allow(clippy::too_many_lines)]
fn render_cost() {
    use std::time::Instant;

    const WORDS: &[&str] = &[
        "the",
        "daemon",
        "replays",
        "**every**",
        "`entry`",
        "日本語",
        "since",
    ];
    let mut msgs = Vec::new();
    for n in 0..2000 {
        let seq = 2 + n * 3;
        let text: Vec<&str> = (0..60)
            .map(|i| WORDS[(usize::try_from(n).unwrap() + i * 7) % WORDS.len()])
            .collect();
        msgs.push(entry(
            seq,
            Body::Input {
                text: text.join(" "),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: None,
            },
        ));
        msgs.push(entry(
            seq + 1,
            Body::Assistant {
                text: text.join(" "),
            },
        ));
        msgs.push(entry(
            seq + 2,
            Body::ToolCall {
                tool: "bash".into(),
                input: Json::encode(&json!({"command": "ls"})).unwrap(),
                id: None,
                card: None,
                by: Audience::Model,
            },
        ));
    }
    let model = CallId::new(2 + 2000 * 3).unwrap();
    msgs.push(entry(
        model.get(),
        Body::ModelCall {
            model: "m".into(),
            attempt: 0,
            choice: protocol::ToolChoice::Auto,
            r#for: None,
            purpose: None,
        },
    ));
    let (mut app, _) = app(Viewport::Full, &msgs);
    // What a frame writes, as the client writes it.
    let term = Term::default();
    let mut screen = Screen::new(Viewport::Full, term.clone());
    let mut frame = |app: &mut App, w: u16| {
        screen.draw(app, (w, 40)).unwrap();
        term.take().1.len()
    };
    let t = Instant::now();
    frame(&mut app, 120);
    let first = t.elapsed();
    let (mut delta, mut bytes) = (Vec::new(), 0);
    let allocs = ALLOCS.load(Relaxed);
    for i in 0..500 {
        let t = Instant::now();
        app.fold(&DaemonMsg::Delta {
            call: model,
            text: format!("tok{i} "),
            attempt: 0,
        });
        bytes += frame(&mut app, 120);
        delta.push(t.elapsed());
    }
    let delta_allocs = (ALLOCS.load(Relaxed) - allocs) / 500;
    let allocs = ALLOCS.load(Relaxed);
    for _ in 0..100 {
        frame(&mut app, 120);
    }
    let idle_allocs = (ALLOCS.load(Relaxed) - allocs) / 100;
    let mut scroll = Vec::new();
    for _ in 0..500 {
        let t = Instant::now();
        app.input(Input::Mouse(Mouse::WheelUp));
        frame(&mut app, 120);
        scroll.push(t.elapsed());
    }
    let t = Instant::now();
    frame(&mut app, 119);
    let rewrap = t.elapsed();
    delta.sort();
    scroll.sort();
    eprintln!(
        "6000 items: first frame {first:?}; delta frame p50 {:?} p99 {:?}, {} bytes, \
         {delta_allocs} allocs (fold and frame); idle frame {idle_allocs} allocs; \
         scroll frame p50 {:?} p99 {:?}; resize (re-wrap all) {rewrap:?}",
        delta[250],
        delta[495],
        bytes / 500,
        scroll[250],
        scroll[495],
    );
}

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

/// Counts allocations for `render_cost`; the system allocator does the work.
struct Counting;

// SAFETY: every call is forwarded to `System` with the same arguments, so
// the contract the caller upheld for this allocator holds for it.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: the caller's `layout`, forwarded.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        // SAFETY: `ptr` came from `System.alloc` with this `layout`.
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn a_slash_lists_the_commands_it_may_become_and_tab_takes_one() {
    let (mut app, _) = app(Viewport::Full, &[DaemonMsg::Synced { seq: 0 }]);
    keys(&mut app, "/s");
    let (_, rows) = frame(&mut app, 70, 20);
    find(&rows, "❯ /sessions");
    find(&rows, "  /steer text");
    assert!(!rows.iter().any(|r| r.contains("/detach")), "{rows:#?}");
    press(&mut app, Key::Down);
    let (_, rows) = frame(&mut app, 70, 20);
    find(&rows, "❯ /steer text");
    assert_eq!(press(&mut app, Key::Tab), None);
    assert_eq!(app.editor.text(), "/steer ");
    let (_, rows) = frame(&mut app, 70, 20);
    assert!(!rows.iter().any(|r| r.contains("/sessions")), "{rows:#?}");
}

#[test]
fn enter_on_a_partial_command_without_arguments_runs_it() {
    let (mut app, _) = app(Viewport::Full, &[DaemonMsg::Synced { seq: 0 }]);
    keys(&mut app, "/deta");
    assert_eq!(press(&mut app, Key::Enter), Some(Effect::Detach));
}

#[test]
fn the_input_sits_between_two_rules() {
    let (mut app, _) = app(Viewport::Full, &[DaemonMsg::Synced { seq: 0 }]);
    let (_, rows) = frame(&mut app, 30, 10);
    let rule = "─".repeat(30);
    assert_eq!(
        (rows[5].as_str(), rows[7].as_str()),
        (rule.as_str(), rule.as_str())
    );
    assert!(rows[6].starts_with("❯"), "{rows:#?}");
}

#[test]
fn esc_denies_an_approval_and_1_approves_it() {
    let (mut app, _) = app(Viewport::Full, &asked_command());
    assert!(matches!(
        press(&mut app, Key::Esc),
        Some(Effect::Cmd(Cmd::Approve {
            approved: false,
            ..
        }))
    ));
    assert!(matches!(
        press(&mut app, Key::Char('1')),
        Some(Effect::Cmd(Cmd::Approve { approved: true, .. }))
    ));
}

/// A running call that nothing waits on: the approved `asked_command`.
fn running_command() -> Vec<DaemonMsg> {
    let mut msgs = asked_command();
    msgs.push(entry(
        5,
        Body::Answer {
            call: CallId::new(3).unwrap(),
            value: Json::encode(&true).unwrap(),
            origin: None,
        },
    ));
    msgs
}

#[test]
fn esc_interrupts_a_running_turn_that_asks_nothing() {
    for viewport in [Viewport::Full, Viewport::Inline] {
        let (mut app, _) = app(viewport, &running_command());
        app.anchor = Some((0, 0));
        assert!(matches!(
            press(&mut app, Key::Esc),
            Some(Effect::Cmd(Cmd::Interrupt))
        ));
    }
}

#[test]
fn enter_confirms_the_highlighted_approval_and_down_moves_it_to_no() {
    let (mut app, _) = app(Viewport::Full, &asked_command());
    let (_, rows) = frame(&mut app, 70, 20);
    find(&rows, "❯ 1. Yes");
    assert!(matches!(
        press(&mut app, Key::Enter),
        Some(Effect::Cmd(Cmd::Approve { approved: true, .. }))
    ));
    press(&mut app, Key::Down);
    let (_, rows) = frame(&mut app, 70, 20);
    find(&rows, "❯ 2. No");
    assert!(matches!(
        press(&mut app, Key::Enter),
        Some(Effect::Cmd(Cmd::Approve {
            approved: false,
            ..
        }))
    ));
}

/// The one timer exists only while a turn works: no tick is asked of the
/// event loop when idle, nor while an approval waits for the person, nor
/// once the call has ended.
#[test]
fn no_timer_runs_unless_a_turn_is_working() {
    let (mut idle, _) = app(Viewport::Full, &[DaemonMsg::Synced { seq: 0 }]);
    assert!(!idle.working());
    frame(&mut idle, 70, 20);
    idle.tick();
    assert!(!idle.needs_frame(), "an idle tick owes no frame");
    let (asked, _) = app(Viewport::Full, &asked_command());
    assert!(!asked.working(), "an approval waits for the person");
    let (mut busy, _) = app(Viewport::Full, &running_command());
    assert!(busy.working());
    let (_, rows) = frame(&mut busy, 70, 20);
    find(&rows, "Working… (0s · esc to interrupt)");
    busy.tick();
    assert!(busy.needs_frame(), "a working turn redraws its clock");
    busy.fold(&entry(
        6,
        Body::ToolResult {
            call: CallId::new(3).unwrap(),
            outcome: Outcome::Done {
                output: Json::encode(&json!({"output": "", "exit": 0})).unwrap(),
            },
            card: None,
        },
    ));
    assert!(!busy.working(), "the turn ended; the timer stops with it");
    let (_, rows) = frame(&mut busy, 70, 20);
    assert!(!rows.iter().any(|r| r.contains("Working")), "{rows:#?}");
    busy.tick();
    assert!(
        !busy.needs_frame(),
        "a finished turn cannot schedule animation"
    );
    assert!(busy.clock.since.is_none());
}

#[test]
fn a_question_mark_on_an_empty_input_opens_help_and_inside_text_is_typed() {
    let (mut app, _) = app(Viewport::Full, &[DaemonMsg::Synced { seq: 0 }]);
    press(&mut app, Key::Char('?'));
    let (_, rows) = frame(&mut app, 80, 40);
    find(&rows, "keys and commands");
    press(&mut app, Key::Char('q'));
    keys(&mut app, "why?");
    assert_eq!(app.editor.text(), "why?");
}

#[test]
fn escape_closes_the_slash_menu_without_losing_the_draft() {
    let (mut app, _) = app(Viewport::Full, &[DaemonMsg::Synced { seq: 0 }]);
    keys(&mut app, "/s");
    assert_eq!(app.menu()[0].0, "/sessions");
    press(&mut app, Key::Esc);
    assert_eq!(app.menu(), [] as [(String, String, String); 0]);
    assert_eq!(app.editor.text(), "/s");
    keys(&mut app, "e");
    assert_eq!(app.menu()[0].0, "/sessions");
}

#[test]
fn running_tools_show_each_progress_update_and_then_the_finished_card() {
    for viewport in [Viewport::Full, Viewport::Inline] {
        let (mut app, _) = app(viewport, &running_command());
        for (seq, text) in [(6, "first\n"), (7, "second\n")] {
            app.fold(&entry(
                seq,
                Body::Progress {
                    call: CallId::new(3).unwrap(),
                    text: text.into(),
                },
            ));
            assert!(app.needs_frame());
            let (_, rows) = frame(&mut app, 80, 24);
            find(&rows, "⎿  first");
            if seq == 7 {
                find(&rows, "second");
            }
        }
        app.fold(&entry(
            8,
            Body::ToolResult {
                call: CallId::new(3).unwrap(),
                outcome: Outcome::Done {
                    output: Json::encode(&json!({"output":"final", "exit":0})).unwrap(),
                },
                card: Some(Card {
                    body: "final".into(),
                    note: "exit 0".into(),
                    ..Card::default()
                }),
            },
        ));
        let (_, rows) = frame(&mut app, 80, 24);
        find(&rows, "⎿  final");
        assert!(!rows.iter().any(|r| r.contains("second")));
    }
}

#[test]
fn inline_review_expands_finished_cards_without_printing_history_twice() {
    let (mut app, _) = app(Viewport::Inline, &running_command());
    app.fold(&entry(
        6,
        Body::ToolResult {
            call: CallId::new(3).unwrap(),
            outcome: Outcome::Done {
                output: Json::encode(&json!({"output":"done", "exit":0})).unwrap(),
            },
            card: Some(Card {
                body: (1..=30).fold(String::new(), |s, n| s + &format!("output {n}\n")),
                note: "exit 0".into(),
                ..Card::default()
            }),
        },
    ));
    assert_ne!(app.scrollback(80).len(), 0);
    assert_eq!(app.committed, app.view().items.len());
    press(&mut app, Key::Ctrl('o'));
    let h = app.height(80, 24);
    let (_, rows) = frame(&mut app, 80, h);
    find(&rows, "output 30");
    press(&mut app, Key::PageUp);
    let (_, rows) = frame(&mut app, 80, h);
    find(&rows, "output 10");
    press(&mut app, Key::Ctrl('o'));
    assert!(app.anchor.is_none());
    assert_eq!(app.scrollback(80).len(), 0);
}

#[test]
fn a_turn_keeps_its_active_time_and_tokens_across_calls_but_not_idle_time() {
    let (mut app, _) = app(
        Viewport::Full,
        &[
            entry(
                1,
                Body::Input {
                    text: "hi".into(),
                    mode: protocol::Mode::Queue,
                    taken: true,
                    origin: None,
                },
            ),
            entry(
                2,
                Body::ModelCall {
                    model: "m".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: None,
                    purpose: None,
                },
            ),
        ],
    );
    app.clock.elapsed = std::time::Duration::from_secs(3);
    app.fold(&entry(3, Body::ModelResult {
        call:CallId::new(2).unwrap(),
        outcome:Outcome::Done {output:Json::encode(&json!({"message":{"content":[{"type":"text","text":"hi"}]},"usage":{"input":12,"output":3}})).unwrap()},
    }));
    assert!(!app.working());
    assert!(app.clock.since.is_none());
    let elapsed = app.clock.elapsed();
    app.fold(&entry(
        4,
        Body::ToolCall {
            tool: "bash".into(),
            input: Json::encode(&json!({"command":"true"})).unwrap(),
            id: None,
            card: None,
            by: Audience::Model,
        },
    ));
    assert_eq!(app.clock.elapsed, elapsed);
    let (_, rows) = frame(&mut app, 80, 24);
    find(&rows, "Working… (3s · ↓ 3 tokens");
    app.fold(&entry(
        5,
        Body::Input {
            text: "next".into(),
            mode: protocol::Mode::Queue,
            taken: true,
            origin: None,
        },
    ));
    assert_eq!(app.clock.before, 3);
    assert_eq!(app.clock.elapsed, std::time::Duration::ZERO);
}

#[test]
fn escape_and_ctrl_c_cancel_submitted_input_before_it_is_published() {
    for viewport in [Viewport::Full, Viewport::Inline] {
        for key in [Key::Esc, Key::Ctrl('c')] {
            let (mut app, _) = app(viewport, &[]);
            app.client.run(Cmd::Input("unpublished".into()));
            assert!(!app.client.turn().running);
            assert!(matches!(
                press(&mut app, key),
                Some(Effect::Cmd(Cmd::Interrupt))
            ));
            let out = app.client.run(Cmd::Interrupt);
            assert!(
                out.iter().any(|e| matches!(
                    e,
                    client::Effect::Io(Io::Send(ClientMsg::Interrupt { .. }))
                ))
            );
        }
    }
}
