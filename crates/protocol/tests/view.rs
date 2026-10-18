//! The shared reducer: properties over generated sessions, and the vectors
//! every client's binding runs too (`vectors/view.json`).

#![allow(clippy::cast_possible_truncation, clippy::too_many_lines)]

use std::path::PathBuf;

use protocol::view::{SessionView, Update};
use protocol::{
    Body, Card, DaemonMsg, Decision, Decode, Entry, Environment, Outcome, Recap, SessionId,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// xorshift64*: deterministic, so a failing seed reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const SESSION: SessionId = SessionId(Uuid::from_u128(0xa));

/// A session as the daemon has it: its log (entries, and entries of kinds
/// this build does not know as `Unknown`), and what a client attached from
/// the start saw live, deltas and asks included.
#[derive(Default)]
struct Session {
    log: Vec<DaemonMsg>,
    live: Vec<DaemonMsg>,
    seq: u64,
}

impl Session {
    fn commit(&mut self, body: Body) -> u64 {
        self.seq += 1;
        let msg = DaemonMsg::Entry(Entry::new(self.seq, 0, body));
        self.log.push(msg.clone());
        self.live.push(msg);
        self.seq
    }

    fn generate(rng: &mut Rng) -> Self {
        let mut s = Self::default();
        for _ in 0..rng.below(24) {
            match rng.below(9) {
                0 => {
                    s.commit(Body::User {
                        text: format!("u{}", rng.below(100)),
                    });
                }
                1 => {
                    s.commit(Body::Assistant {
                        text: format!("a{}", rng.below(100)),
                    });
                }
                2 => s.tool(rng),
                3 => s.model(rng),
                4 => {
                    s.commit(Body::Hook {
                        hook: "policy".into(),
                        decision: Decision::Block {
                            tool: "t".into(),
                            reason: "no".into(),
                        },
                    });
                }
                5 => s.child(rng),
                6 => {
                    s.commit(Body::Environment(Environment {
                        cwd: format!("/w{}", rng.below(3)).into(),
                        ..Environment::default()
                    }));
                }
                7 => {
                    s.seq += 1;
                    let msg = DaemonMsg::Unknown { seq: Some(s.seq) };
                    s.log.push(msg.clone());
                    s.live.push(msg);
                }
                _ => {
                    s.live.push(DaemonMsg::Error {
                        message: "e".into(),
                    });
                }
            }
        }
        s
    }

    fn tool(&mut self, rng: &mut Rng) {
        let input = json!({ "command": format!("echo {}", rng.below(10)) });
        let call = self.commit(Body::ToolCall {
            tool: "run_command".into(),
            input: input.clone(),
            id: None,
            card: None,
        });
        if rng.chance(50) {
            self.commit(Body::Ask { call });
            self.live.push(DaemonMsg::Ask {
                call,
                tool: "run_command".into(),
                input,
            });
            if rng.chance(30) {
                return;
            }
            self.commit(Body::Hook {
                hook: "user".into(),
                decision: Decision::Approval {
                    tool: "run_command".into(),
                    approved: rng.chance(50),
                    call: Some(call),
                },
            });
        }
        if rng.chance(50) {
            self.commit(Body::ToolProcess {
                call,
                pgid: 7,
                boot: "b".into(),
                background: false,
            });
        }
        if rng.chance(80) {
            let outcome = outcome(rng, json!({ "stdout": "x\n", "stderr": "", "exit": 0 }));
            self.commit(Body::ToolResult {
                call,
                outcome,
                card: None,
            });
        }
    }

    fn model(&mut self, rng: &mut Rng) {
        let call = self.commit(Body::ModelCall { model: "m".into() });
        let text = format!("reply {}", rng.below(1000));
        let mut rest = text.as_str();
        while !rest.is_empty() {
            // ASCII, so any cut is a char boundary.
            let at = (1 + rng.below(4) as usize).min(rest.len());
            let (chunk, tail) = rest.split_at(at);
            self.live.push(DaemonMsg::Delta {
                call,
                text: chunk.into(),
            });
            rest = tail;
        }
        if rng.chance(80) {
            let output = json!({
                "message": { "content": [{ "type": "text", "text": text }] },
                "usage": { "input": 3, "output": 5 },
            });
            let outcome = outcome(rng, output);
            self.commit(Body::ModelResult { call, outcome });
        }
    }

    fn child(&mut self, rng: &mut Rng) {
        let child = SessionId(Uuid::from_u128(u128::from(rng.next())));
        self.commit(Body::Link {
            child,
            input: "go".into(),
        });
        if rng.chance(70) {
            let watch = self.commit(Body::Watch { child });
            if rng.chance(70) {
                let outcome = outcome(rng, json!({ "slept": 1 }));
                self.commit(Body::Fired { watch, outcome });
            }
        }
    }

    /// What an attach after `since` is sent, as the daemon sends it: the
    /// entries after it, `Synced`, a recap, and every ask still waiting.
    fn attach(&self, since: u64, upto: u64) -> Vec<DaemonMsg> {
        let mut msgs = vec![DaemonMsg::Attached {
            session: SESSION,
            workspace: "/w".into(),
        }];
        let seq = |m: &DaemonMsg| match m {
            DaemonMsg::Entry(e) => e.seq,
            DaemonMsg::Unknown { seq } => seq.unwrap(),
            _ => unreachable!(),
        };
        let log: Vec<_> = self.log.iter().filter(|m| seq(m) <= upto).collect();
        msgs.extend(log.iter().filter(|m| seq(m) > since).map(|&m| m.clone()));
        msgs.push(DaemonMsg::Synced { seq: upto });
        msgs.push(DaemonMsg::Recap(Recap::default()));
        let entries = || {
            log.iter().filter_map(|m| match m {
                DaemonMsg::Entry(e) => Some(e),
                _ => None,
            })
        };
        for e in entries() {
            if let Body::Ask { call } = e.body {
                let answered = entries().any(|a| {
                    matches!(a.body, Body::Hook { decision: Decision::Approval { call: Some(c), .. }, .. } if c == call)
                });
                let Some(Body::ToolCall { tool, input, .. }) =
                    entries().find(|t| t.seq == call).map(|t| &t.body)
                else {
                    unreachable!()
                };
                if !answered {
                    msgs.push(DaemonMsg::Ask {
                        call,
                        tool: tool.clone(),
                        input: input.clone(),
                    });
                }
            }
        }
        msgs
    }

    /// The live stream as a client attached from the start sees it.
    fn watched(&self) -> Vec<DaemonMsg> {
        let mut msgs = self.attach(0, 0);
        msgs.extend(self.live.iter().cloned());
        msgs
    }
}

fn outcome(rng: &mut Rng, output: Value) -> Outcome {
    match rng.below(5) {
        0 => Outcome::Failed { error: "f".into() },
        1 => Outcome::Interrupted { output: None },
        _ => Outcome::Done { output },
    }
}

/// Folds `msgs` into `view`, checking each update's bookkeeping.
fn fold(view: &mut SessionView, msgs: &[DaemonMsg]) -> Vec<Update> {
    let mut updates = Vec::new();
    for msg in msgs {
        let rev = view.rev;
        if let Some(update) = view.fold(msg) {
            assert_eq!(update.rev, rev + 1, "revisions count updates");
            assert_eq!(update.seq, view.seq);
            updates.push(update);
        } else {
            assert_eq!(view.rev, rev);
        }
    }
    updates
}

fn folded(msgs: &[DaemonMsg]) -> SessionView {
    let mut view = SessionView::default();
    fold(&mut view, msgs);
    view
}

fn assert_same(a: &SessionView, b: &SessionView, seed: u64) {
    assert_eq!(
        serde_json::to_value(a).unwrap(),
        serde_json::to_value(b).unwrap(),
        "seed {seed}"
    );
    assert_eq!(a.state_hash(), b.state_hash(), "seed {seed}");
    assert_eq!(
        a.asked().collect::<Vec<_>>(),
        b.asked().collect::<Vec<_>>(),
        "seed {seed}"
    );
}

const SEEDS: u64 = 500;

#[test]
fn folding_a_replay_equals_folding_live() {
    for seed in 1..=SEEDS {
        let s = Session::generate(&mut Rng(seed));
        let live = folded(&s.watched());
        let replayed = folded(&s.attach(0, s.seq));
        assert_eq!(live.seq, s.seq);
        assert_same(&live, &replayed, seed);
        assert!(live.streaming.is_none() || replayed.streaming.is_none());
    }
}

/// A client that drops off at any point and reattaches from any seq it has
/// already passed ends where one that never left does.
#[test]
fn a_reconnect_replay_from_any_seq_seen_is_idempotent() {
    for seed in 1..=SEEDS {
        let mut rng = Rng(seed);
        let s = Session::generate(&mut rng);
        let watched = s.watched();
        let whole = folded(&watched);

        let cut = rng.below(watched.len() as u64 + 1) as usize;
        let resume = cut + rng.below((watched.len() - cut) as u64 + 1) as usize;
        let mut view = folded(&watched[..cut]);
        fold(&mut view, &[DaemonMsg::Draining]);
        let since = rng.below(view.seq + 1);
        let upto = watched[..resume]
            .iter()
            .filter_map(|m| match m {
                DaemonMsg::Entry(e) => Some(e.seq),
                DaemonMsg::Unknown { seq } => *seq,
                _ => None,
            })
            .max()
            .unwrap_or(0);
        fold(&mut view, &s.attach(since, upto));
        fold(&mut view, &watched[resume..]);
        assert_same(&view, &whole, seed);
    }
}

#[test]
fn folding_a_message_twice_changes_nothing_the_second_time() {
    for seed in 1..=SEEDS {
        let s = Session::generate(&mut Rng(seed));
        let mut view = SessionView::default();
        for msg in s.watched() {
            view.fold(&msg);
            if matches!(msg, DaemonMsg::Entry(_) | DaemonMsg::Ask { .. }) {
                assert_eq!(view.fold(&msg), None, "seed {seed}: {msg:?}");
            }
        }
    }
}

#[test]
fn attaching_to_another_session_starts_over() {
    let s = Session::generate(&mut Rng(3));
    let mut view = folded(&s.watched());
    let other = SessionId(Uuid::from_u128(0xb));
    fold(
        &mut view,
        &[DaemonMsg::Attached {
            session: other,
            workspace: "/v".into(),
        }],
    );
    assert_eq!(view.session, Some(other));
    assert_eq!(view.items, []);
    assert_eq!(view.seq, 0);
}

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/view.json")
}

/// Named scenarios: the daemon messages a client receives, as JSON.
fn scenarios() -> Vec<(&'static str, Vec<Value>)> {
    let msg = |m: DaemonMsg| serde_json::to_value(m).unwrap();
    let entry = |seq, body| msg(DaemonMsg::Entry(Entry::new(seq, 0, body)));
    let attached = || {
        msg(DaemonMsg::Attached {
            session: SESSION,
            workspace: "/w".into(),
        })
    };
    let command = json!({ "command": "echo hi" });
    let generated = |seed| {
        let s = Session::generate(&mut Rng(seed));
        s.watched()
            .iter()
            .map(|m| match m {
                DaemonMsg::Unknown { seq } => json!({
                    "t": "entry", "seq": seq, "epoch": 0, "kind": "from_a_newer_build",
                }),
                m => serde_json::to_value(m).unwrap(),
            })
            .collect::<Vec<_>>()
    };
    vec![
        (
            "a streamed reply, then the committed one",
            vec![
                attached(),
                msg(DaemonMsg::Synced { seq: 0 }),
                entry(1, Body::User { text: "hi".into() }),
                entry(2, Body::ModelCall { model: "m".into() }),
                msg(DaemonMsg::Delta {
                    call: 2,
                    text: "hel".into(),
                }),
                msg(DaemonMsg::Delta {
                    call: 2,
                    text: "lo".into(),
                }),
                entry(
                    3,
                    Body::ModelResult {
                        call: 2,
                        outcome: Outcome::Done {
                            output: json!({
                                "message": { "content": [{ "type": "text", "text": "hello" }] },
                                "usage": { "input": 4, "output": 2 },
                            }),
                        },
                    },
                ),
            ],
        ),
        (
            "an approval asked, re-delivered on attach, and answered",
            vec![
                attached(),
                entry(
                    1,
                    Body::ToolCall {
                        tool: "run_command".into(),
                        input: command.clone(),
                        id: None,
                        card: None,
                    },
                ),
                entry(2, Body::Ask { call: 1 }),
                msg(DaemonMsg::Synced { seq: 2 }),
                msg(DaemonMsg::Recap(Recap {
                    tools: vec!["run_command".into()],
                    asks: 1,
                    ..Recap::default()
                })),
                msg(DaemonMsg::Ask {
                    call: 1,
                    tool: "run_command".into(),
                    input: command.clone(),
                }),
                msg(DaemonMsg::Ask {
                    call: 1,
                    tool: "run_command".into(),
                    input: command,
                }),
                entry(
                    3,
                    Body::Hook {
                        hook: "user".into(),
                        decision: Decision::Approval {
                            tool: "run_command".into(),
                            approved: true,
                            call: Some(1),
                        },
                    },
                ),
                entry(
                    4,
                    Body::ToolResult {
                        call: 1,
                        outcome: Outcome::Done {
                            output: json!({ "stdout": "hi\n", "stderr": "", "exit": 0 }),
                        },
                        card: None,
                    },
                ),
            ],
        ),
        (
            "a child, its watch, and the watch firing",
            vec![
                attached(),
                entry(
                    1,
                    Body::Link {
                        child: SessionId(Uuid::from_u128(0xc)),
                        input: "sleep 1".into(),
                    },
                ),
                entry(
                    2,
                    Body::Watch {
                        child: SessionId(Uuid::from_u128(0xc)),
                    },
                ),
                entry(
                    3,
                    Body::Fired {
                        watch: 2,
                        outcome: Outcome::Done {
                            output: json!({ "slept": 1 }),
                        },
                    },
                ),
                msg(DaemonMsg::Synced { seq: 3 }),
            ],
        ),
        (
            "a drain, then a reconnect replaying entries already seen",
            vec![
                attached(),
                entry(1, Body::User { text: "one".into() }),
                entry(2, Body::Assistant { text: "one".into() }),
                msg(DaemonMsg::Synced { seq: 2 }),
                msg(DaemonMsg::Draining),
                attached(),
                entry(2, Body::Assistant { text: "one".into() }),
                entry(3, Body::User { text: "two".into() }),
                msg(DaemonMsg::Synced { seq: 3 }),
            ],
        ),
        (
            "kinds from a newer build are skipped",
            vec![
                attached(),
                json!({ "t": "presence", "who": 1 }),
                json!({ "t": "entry", "seq": 1, "epoch": 0, "kind": "from_a_newer_build", "x": [] }),
                entry(
                    2,
                    Body::Environment(Environment {
                        cwd: "/w".into(),
                        ..Environment::default()
                    }),
                ),
                msg(DaemonMsg::Synced { seq: 2 }),
            ],
        ),
        (
            "a tool's cards, then a slash command",
            vec![
                attached(),
                entry(
                    1,
                    Body::ToolCall {
                        tool: "bash".into(),
                        input: json!({ "command": "echo hi" }),
                        id: None,
                        card: Some(Card {
                            title: "$ echo hi".into(),
                            ..Card::default()
                        }),
                    },
                ),
                entry(
                    2,
                    Body::ToolResult {
                        call: 1,
                        outcome: Outcome::Done {
                            output: json!({ "exit": 0, "output": "hi\n" }),
                        },
                        card: Some(Card {
                            title: "$ echo hi".into(),
                            body: "hi\n".into(),
                            note: "exit 0".into(),
                        }),
                    },
                ),
                entry(
                    3,
                    Body::Ext {
                        ext: "shell".into(),
                        state: "cwd".into(),
                        data: json!({ "cwd": "/w/src" }),
                    },
                ),
                entry(
                    4,
                    Body::Command {
                        name: "jobs".into(),
                        args: String::new(),
                    },
                ),
                entry(
                    5,
                    Body::CommandResult {
                        call: 4,
                        outcome: Outcome::Done {
                            output: "no jobs".into(),
                        },
                    },
                ),
                msg(DaemonMsg::Synced { seq: 5 }),
            ],
        ),
        ("generated session 7", generated(7)),
        ("generated session 11", generated(11)),
    ]
}

/// Folds each vector's messages and compares the updates, the state and
/// its hash with the file; `UPDATE_VECTORS=1` rewrites the file instead.
#[test]
fn vectors() {
    let computed: Vec<Value> = scenarios()
        .into_iter()
        .map(|(name, messages)| {
            let mut view = SessionView::default();
            let updates: Vec<_> = messages
                .iter()
                .filter_map(|m| {
                    let msg = DaemonMsg::from_json(m.to_string().as_bytes()).unwrap();
                    view.fold(&msg)
                })
                .collect();
            json!({
                "name": name,
                "messages": messages,
                "updates": updates,
                "state": view,
                "hash": format!("{:016x}", view.state_hash()),
            })
        })
        .collect();
    let path = vectors_path();
    if std::env::var_os("UPDATE_VECTORS").is_some() {
        let mut text = serde_json::to_string_pretty(&computed).unwrap();
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        return;
    }
    let stored: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        stored.len(),
        computed.len(),
        "UPDATE_VECTORS=1 to regenerate"
    );
    for (stored, computed) in stored.iter().zip(&computed) {
        // The file's own messages are the input a binding folds.
        assert_eq!(
            stored["messages"], computed["messages"],
            "{}",
            stored["name"]
        );
        assert_eq!(stored, computed, "{}", stored["name"]);
    }
}

#[test]
fn clients_describe_calls_and_recaps_alike() {
    use protocol::view::{call_title, output_text, recap_line};

    let ls = json!({"command": "ls"});
    let card = Card {
        title: "$ ls -a".into(),
        body: "a\n".into(),
        note: "exit 0".into(),
    };
    assert_eq!(call_title("bash", &ls, Some(&card)), "$ ls -a");
    assert_eq!(call_title("bash", &ls, None), "$ ls");
    assert_eq!(
        call_title("edit", &json!({"path": "a"}), None),
        r#"edit {"path":"a"}"#
    );
    let shown = |output: &Value, card| output_text(output, card);
    assert_eq!(
        shown(&json!({}), Some(&card)),
        Some(("a\n".into(), "exit 0".into()))
    );
    assert_eq!(
        shown(&json!({"stdout": "o\n", "stderr": "e\n", "exit": 2}), None),
        Some(("o\ne\n".into(), "exit 2".into()))
    );
    assert_eq!(shown(&json!("text"), Some(&Card::default())), None);
    let recap = Recap {
        prompt: Some("hi".into()),
        tools: vec!["bash".into(); 4],
        asks: 1,
        reply: Some("done".into()),
        ..Recap::default()
    };
    assert_eq!(
        recap_line(&recap),
        "recap · you: hi · ran 4 tools · a: done · 1 waiting for approval"
    );
    assert_eq!(recap_line(&Recap::default()), "recap");
}
