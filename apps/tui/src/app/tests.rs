use std::path::PathBuf;

use protocol::{
    Activity, Body, Card, ClientMsg, DaemonMsg, Entry, Outcome, Recap, Residency, SessionId,
    SessionSummary, Target,
};
use serde_json::json;

use super::{App, Effect};
use crate::keys::{Input, Key, Mouse};
use crate::render::{Buf, Viewport};
use crate::text::Line;

fn ws() -> PathBuf {
    PathBuf::from("/work/project")
}

fn entry(seq: u64, body: Body) -> DaemonMsg {
    DaemonMsg::Entry(Entry::new(seq, 0, body))
}

/// An app attached to a fresh session, with `msgs` folded in.
fn app(viewport: Viewport, msgs: &[DaemonMsg]) -> (App, SessionId) {
    let session = SessionId::new();
    let mut app = App::new(viewport, ws(), &Target::New);
    app.conn = super::Conn::Open { remote: false };
    app.fold(&DaemonMsg::Attached {
        session,
        workspace: ws(),
    });
    for msg in msgs {
        app.fold(msg);
    }
    (app, session)
}

fn frame(app: &mut App, w: u16, h: u16) -> (Buf, Vec<String>) {
    let mut buf = Buf::new(w, h);
    app.draw(&mut buf);
    let rows = buf.rows();
    (buf, rows)
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

fn asked_command() -> Vec<DaemonMsg> {
    vec![
        entry(
            2,
            Body::User {
                text: "$ ls".into(),
            },
        ),
        entry(
            3,
            Body::ToolCall {
                tool: "bash".into(),
                input: json!({"command": "ls"}),
                id: None,
                card: Some(Card {
                    title: "$ ls".into(),
                    ..Card::default()
                }),
            },
        ),
        entry(4, Body::Ask { call: 3 }),
        DaemonMsg::Synced { seq: 4 },
        DaemonMsg::Ask {
            call: 3,
            tool: "bash".into(),
            input: json!({"command": "ls"}),
        },
    ]
}

#[test]
fn an_approval_shows_buttons_and_y_or_a_click_answers_it() {
    let (mut app, _) = app(Viewport::Full, &asked_command());
    let (_, rows) = frame(&mut app, 70, 20);
    assert!(rows.iter().any(|r| r == "› $ ls"), "{rows:#?}");
    find(&rows, "• $ ls  ? waiting for approval");
    let bar = find(&rows, "? approve $ ls");
    assert_eq!(bar.1, 16, "above the input: {rows:#?}");
    assert!(matches!(
        press(&mut app, Key::Char('y')),
        Some(Effect::Send(ClientMsg::Approve {
            call: 3,
            approved: true,
            ..
        }))
    ));
    let (x, y) = find(&rows, "[ n  no ]");
    assert!(matches!(
        app.input(Input::Mouse(Mouse::Press { x: x + 2, y })),
        Some(Effect::Send(ClientMsg::Approve {
            call: 3,
            approved: false,
            ..
        }))
    ));
    // With text in the input, y is typed.
    press(&mut app, Key::Char('x'));
    assert!(press(&mut app, Key::Char('y')).is_none());
}

#[test]
fn a_streamed_reply_shows_live_then_its_committed_text() {
    let (mut app, _) = app(
        Viewport::Full,
        &[
            entry(2, Body::User { text: "hi".into() }),
            entry(
                3,
                Body::ModelCall {
                    model: "gpt".into(),
                },
            ),
            DaemonMsg::Synced { seq: 3 },
            DaemonMsg::Delta {
                call: 3,
                text: "Hel".into(),
            },
            DaemonMsg::Delta {
                call: 3,
                text: "lo **you**".into(),
            },
        ],
    );
    let (buf, rows) = frame(&mut app, 50, 12);
    let (x, y) = find(&rows, "Hello you▍");
    assert!(buf.style_at(x + 6, y).bold, "markdown bold");
    find(&rows, "● live");
    app.fold(&entry(
        4,
        Body::ModelResult {
            call: 3,
            outcome: Outcome::Done {
                output: json!({"message": {"content": [{"type": "text", "text": "Hello you!"}]},
                               "usage": {"input": 12, "output": 3}}),
            },
        },
    ));
    let (_, rows) = frame(&mut app, 50, 12);
    find(&rows, "  Hello you!");
    assert!(!rows.iter().any(|r| r.contains('▍')));
    find(&rows, "gpt  ↑12 ↓3");
}

#[test]
fn a_card_folds_on_click_and_ctrl_o_shows_all_output() {
    let stdout = (1..=30).fold(String::new(), |s, n| s + &format!("out {n}\n"));
    let mut msgs = asked_command();
    msgs.push(entry(
        5,
        Body::ToolResult {
            call: 3,
            outcome: Outcome::Done {
                output: json!({"output": stdout, "exit": 0, "cwd": "/w", "duration_ms": 1}),
            },
            card: Some(Card {
                title: "$ ls".into(),
                body: stdout.clone(),
                note: "exit 0".into(),
            }),
        },
    ));
    let (mut app, _) = app(Viewport::Full, &msgs);
    let (_, rows) = frame(&mut app, 60, 40);
    find(&rows, "⋯ 20 earlier lines");
    let (x, y) = find(&rows, "▾ $ ls  ✓ exit 0");
    assert!(app.input(Input::Mouse(Mouse::Press { x, y })).is_none());
    let (_, rows) = frame(&mut app, 60, 40);
    find(&rows, "▸ $ ls  ✓ exit 0");
    find(&rows, "│ 30 lines");
    let (x, y) = find(&rows, "▸ $ ls");
    app.input(Input::Mouse(Mouse::Press { x, y }));
    press(&mut app, Key::Ctrl('o'));
    let (_, rows) = frame(&mut app, 60, 40);
    find(&rows, "│ out 1");
}

#[test]
fn the_wheel_scrolls_and_new_rows_keep_a_scrolled_view_still() {
    let msgs: Vec<_> = (2..40)
        .map(|seq| {
            entry(
                seq,
                Body::User {
                    text: format!("message {seq}"),
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
    find(&rows, "newer below");
    app.fold(&entry(
        40,
        Body::User {
            text: "message 40".into(),
        },
    ));
    let (_, rows) = frame(&mut app, 40, 12);
    assert_eq!(rows[0], top, "the view did not move");
    press(&mut app, Key::Esc);
    let (_, rows) = frame(&mut app, 40, 12);
    find(&rows, "message 40");
}

#[test]
fn the_empty_input_hint_starts_after_the_caret() {
    let (mut app, _) = app(Viewport::Full, &[]);
    let (buf, rows) = frame(&mut app, 60, 10);
    let (x, y) = find(&rows, "message the agent");
    assert_eq!(buf.cursor, Some((x - 1, y)));
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
    let mut unattached = App::new(Viewport::Full, ws(), &Target::New);
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
        Body::Hook {
            hook: "user".into(),
            decision: protocol::Decision::Approval {
                tool: "bash".into(),
                approved: true,
                call: Some(3),
            },
        },
    ));
    let (mut app, _) = app(Viewport::Full, &msgs);
    press(&mut app, Key::Char('x'));
    assert!(matches!(
        press(&mut app, Key::Ctrl('c')),
        Some(Effect::Send(ClientMsg::Interrupt { .. }))
    ));
    assert!(!app.editor.is_empty(), "the draft stays");
    app.fold(&entry(
        6,
        Body::ToolResult {
            call: 3,
            outcome: Outcome::Done {
                output: json!({"output": "", "exit": null, "signal": "SIGTERM", "aborted": true}),
            },
            card: Some(Card {
                title: "$ ls".into(),
                body: String::new(),
                note: "aborted · killed by SIGTERM".into(),
            }),
        },
    ));
    let (_, rows) = frame(&mut app, 60, 12);
    find(&rows, "• $ ls  ✗ aborted · killed by SIGTERM");
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
        entry(1, Body::User { text: "hi".into() }),
        entry(2, Body::ModelCall { model: "m".into() }),
    ];
    let (mut app, _) = app(Viewport::Full, &msgs);
    assert!(matches!(
        press(&mut app, Key::Ctrl('c')),
        Some(Effect::Send(ClientMsg::Interrupt { client_seq: None }))
    ));
    app.fold(&entry(
        3,
        Body::ModelResult {
            call: 2,
            outcome: Outcome::Interrupted {
                output: Some("so far".into()),
            },
        },
    ));
    assert!(!app.running());
}

#[test]
fn slash_commands() {
    let (mut app, current) = app(Viewport::Full, &[]);
    assert!(matches!(typed(&mut app, "/detach"), Some(Effect::Detach)));
    assert!(matches!(
        typed(&mut app, "/env refresh"),
        Some(Effect::RefreshEnv)
    ));
    assert!(matches!(
        typed(&mut app, "/new"),
        Some(Effect::Open(Target::New))
    ));
    // The daemon runs the rest, or says there is no such command.
    assert!(matches!(
        typed(&mut app, "/nope"),
        Some(Effect::Send(ClientMsg::Input { text, .. })) if text == "/nope"
    ));
    assert!(matches!(
        typed(&mut app, "/interrupt"),
        Some(Effect::Send(ClientMsg::Interrupt { .. }))
    ));
    assert!(matches!(
        typed(&mut app, "hello there"),
        Some(Effect::Send(ClientMsg::Input { text, .. })) if text == "hello there"
    ));
    app.conn = super::Conn::Open { remote: true };
    assert!(typed(&mut app, "/env refresh").is_none());

    assert!(matches!(typed(&mut app, "/sessions"), Some(Effect::List)));
    let other = SessionId::new();
    let summary = |id, workspace: &str, activity| SessionSummary {
        id,
        workspace: workspace.into(),
        entries: 3,
        clients: 0,
        activity,
        residency: Residency::Loaded,
    };
    app.fold(&DaemonMsg::Sessions {
        sessions: vec![
            summary(other, "/elsewhere", Activity::Idle),
            summary(current, "/work/project", Activity::Approval),
        ],
    });
    let (_, rows) = frame(&mut app, 80, 14);
    let (_, here) = find(&rows, &format!("• {}  approval   loaded", current.short()));
    let (x, there) = find(&rows, &format!("{}  idle", other.short()));
    assert!(here < there, "newest first");
    assert!(matches!(
        app.input(Input::Mouse(Mouse::Press { x, y: there })),
        Some(Effect::Open(Target::Id(id))) if id == other
    ));
}

#[test]
fn resuming_where_there_is_no_session_opens_the_picker() {
    let mut app = App::new(
        Viewport::Full,
        ws(),
        &Target::Latest {
            any_workspace: false,
        },
    );
    let effect = app.fold(&DaemonMsg::Error {
        message: "no matching session".into(),
    });
    assert!(matches!(effect, Some(Effect::List)));
    let (_, rows) = frame(&mut app, 60, 12);
    find(&rows, "sessions");
    assert!(matches!(
        press(&mut app, Key::Char('n')),
        Some(Effect::Open(Target::New))
    ));
}

#[test]
fn the_recap_shows_until_dismissed() {
    let recap = Recap {
        prompt: Some("$ ls".into()),
        tools: vec!["bash".into()],
        asks: 1,
        ..Recap::default()
    };
    let (mut app, _) = app(Viewport::Full, &[DaemonMsg::Recap(recap)]);
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
    assert!(printed.contains(&"› $ ls".to_owned()), "{printed:?}");
    assert!(app.scrollback(60).is_empty(), "printed once");
    let height = app.height(60, 40);
    let (_, rows) = frame(&mut app, 60, height);
    find(&rows, "? waiting for approval");
    assert!(!rows.iter().any(|r| r == "› $ ls"), "{rows:#?}");
    app.fold(&entry(
        5,
        Body::ToolResult {
            call: 3,
            // As older logs hold a command's result.
            outcome: Outcome::Done {
                output: json!({"stdout": "a\n", "stderr": "", "exit": 0}),
            },
            card: None,
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

/// Render cost on a long transcript, measured rather than checked:
/// `cargo test --release -p tui -- --ignored --nocapture render_cost`.
#[test]
#[ignore = "a measurement"]
fn render_cost() {
    use std::time::Instant;

    use crate::render::Screen;

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
            Body::User {
                text: text.join(" "),
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
                input: json!({"command": "ls"}),
                id: None,
                card: None,
            },
        ));
    }
    let model = 2 + 2000 * 3;
    msgs.push(entry(model, Body::ModelCall { model: "m".into() }));
    let (mut app, _) = app(Viewport::Full, &msgs);
    let mut screen = Screen::new(Viewport::Full);
    let mut frame = |app: &mut App, w: u16| {
        let mut buf = screen.frame(w, 40);
        app.draw(&mut buf);
        screen.render(buf).len()
    };
    let t = Instant::now();
    frame(&mut app, 120);
    let first = t.elapsed();
    let (mut delta, mut bytes) = (Vec::new(), 0);
    for i in 0..500 {
        let t = Instant::now();
        app.fold(&DaemonMsg::Delta {
            call: model,
            text: format!("tok{i} "),
        });
        bytes += frame(&mut app, 120);
        delta.push(t.elapsed());
    }
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
        "6000 items: first frame {first:?}; delta frame p50 {:?} p99 {:?}, {} bytes; \
         scroll frame p50 {:?} p99 {:?}; resize (re-wrap all) {rewrap:?}",
        delta[250],
        delta[495],
        bytes / 500,
        scroll[250],
        scroll[495],
    );
}
