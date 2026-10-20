use std::path::PathBuf;

use protocol::{
    Activity, Body, Card, ClientMsg, DaemonMsg, Entry, Outcome, Residency, SessionId,
    SessionSummary, Target,
};
use ratatui_core::buffer::Buffer;
use ratatui_core::style::Modifier;
use serde_json::json;

use client::{Cmd, Event, Welcome};
use protocol::{Audience, Caps, Json};

use super::{App, Effect};
use crate::keys::{Input, Key, Mouse};
use crate::render::{Viewport, rows};
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
    let effects = app.client.open(Target::New);
    app.absorb(effects);
    let id = app.client.connection();
    app.event(Event::Connected {
        id,
        welcome: Welcome {
            proto: 1,
            caps: Caps::ALL,
        },
        remote,
        commands: Vec::new(),
    });
}

/// A frame: its cells, where the caret shows (if it does), and its rows.
struct Screen {
    buf: Buffer,
    caret: Option<(u16, u16)>,
}

/// Draws a frame, and checks it is what drawing it on a blank canvas gives:
/// the rows kept from the frame before are the ones it would draw.
fn frame(app: &mut App, w: u16, h: u16) -> (Screen, Vec<String>) {
    let caret = app.draw(w, h);
    let buf = app.canvas().buffer().clone();
    let kept = std::mem::take(&mut app.canvas);
    assert_eq!(app.draw(w, h), caret);
    assert_eq!(app.canvas().buffer(), &buf, "kept rows are stale");
    app.canvas = kept;
    let rows = rows(&buf);
    (Screen { buf, caret }, rows)
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
                call: protocol::CallId::new(3),
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
    assert!(rows.iter().any(|r| r == "› $ ls"), "{rows:#?}");
    find(&rows, "• $ ls  ? waiting for approval");
    let bar = find(&rows, "? approve $ ls");
    assert_eq!(bar.1, 16, "above the input: {rows:#?}");
    assert!(matches!(
        press(&mut app, Key::Char('y')),
        Some(Effect::Cmd(Cmd::Approve {
            call: 3,
            approved: true,
        }))
    ));
    let (x, y) = find(&rows, "[ n  no ]");
    assert!(matches!(
        app.input(Input::Mouse(Mouse::Press { x: x + 2, y })),
        Some(Effect::Cmd(Cmd::Approve {
            call: 3,
            approved: false,
        }))
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
                call: 3,
                text: "Hel".into(),
                attempt: 0,
            },
            DaemonMsg::Delta {
                call: 3,
                text: "lo **you**".into(),
                attempt: 0,
            },
        ],
    );
    let (screen, rows) = frame(&mut app, 50, 12);
    let (x, y) = find(&rows, "Hello you▍");
    assert!(
        screen.buf[(x + 6, y)].modifier.contains(Modifier::BOLD),
        "markdown bold"
    );
    find(&rows, "● live");
    app.fold(&entry(
        4,
        Body::ModelResult {
            call: 3,
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
    find(&rows, "  Hello you!");
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
                    call: 3,
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
            call: 3,
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
    find(&rows, "newer below");
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
fn the_empty_input_hint_starts_after_the_caret() {
    let (mut app, _) = app(Viewport::Full, &[]);
    let (screen, rows) = frame(&mut app, 60, 10);
    let (x, y) = find(&rows, "message the agent");
    assert_eq!(screen.caret, Some((x - 1, y)));
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
        Body::Answer {
            call: protocol::CallId::new(3).unwrap(),
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
            call: 3,
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
            call: 2,
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
    assert!(app.command(Cmd::RefreshEnv).is_empty());
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
        Some(Effect::Cmd(Cmd::Open(Target::Id(id)))) if id == other
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
    connect(&mut app, false);
    let effects = app.fold(&DaemonMsg::Error {
        message: "no matching session".into(),
    });
    assert!(
        matches!(&effects[..], [client::Effect::Send(ClientMsg::List)]),
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
            outcome: Outcome::Done {
                output: Json::encode(&json!({"output": "a\n", "exit": 0})).unwrap(),
            },
            card: Some(Card {
                title: "$ ls".into(),
                body: "a\n".into(),
                note: "exit 0".into(),
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

/// Render cost on a long transcript, measured rather than checked:
/// `cargo test --release -p tui -- --ignored --nocapture render_cost`.
#[test]
#[ignore = "a measurement"]
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
    let model = 2 + 2000 * 3;
    msgs.push(entry(
        model,
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
    let count = Count::default();
    let mut screen = crate::screen::Screen::new(Viewport::Full, count.clone());
    let mut frame = |app: &mut App, w: u16| {
        let before = count.0.get();
        screen.draw(app, (w, 40)).unwrap();
        count.0.get() - before
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
            attempt: 0,
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

/// Counts the bytes a terminal is sent.
#[derive(Clone, Default)]
struct Count(std::rc::Rc<std::cell::Cell<usize>>);

impl std::io::Write for Count {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.set(self.0.get() + buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
