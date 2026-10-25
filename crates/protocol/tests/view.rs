//! The shared reducer: properties over generated sessions, and the vectors
//! every client's binding runs too (`vectors/view.json`).

#![allow(clippy::cast_possible_truncation, clippy::too_many_lines)]

use std::path::PathBuf;

use protocol::publication::Publication;
use protocol::view::{Change, Recap, SessionView, Shown, Update};
use protocol::{
    Audience, Body, CallId, Card, DaemonMsg, Decode, Entry, Environment, Epoch, Json, Mode,
    Outcome, Permission, Seq, SessionId, Verdict,
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

#[test]
fn answered_questions_discard_late_annotations() {
    let (_, messages) = scenarios()
        .into_iter()
        .find(|(name, _)| *name == "approval reason precedes its optional description")
        .unwrap();
    let mut view = SessionView::default();
    for value in messages {
        let mut msg = DaemonMsg::from_json(value.to_string().as_bytes()).unwrap();
        if let DaemonMsg::Entry(entry) = &mut msg
            && matches!(entry.body, Body::Fact { .. })
        {
            view.fold(&DaemonMsg::Entry(Entry::new(
                entry.seq,
                Epoch(0),
                Body::Answer {
                    call: CallId::new(1).unwrap(),
                    value: Json::encode(&true).unwrap(),
                    origin: None,
                },
            )));
            entry.seq = Seq::new(entry.seq.get() + 1).unwrap();
            view.fold(&msg);
            let shown = view.present(&view.items[0]);
            assert_eq!(shown.buttons, protocol::view::Buttons::None);
            assert!(
                shown.description.is_none(),
                "an answered question acquired a late description: {:?}",
                shown.description
            );
            return;
        }
        view.fold(&msg);
    }
    panic!("scenario has no description result");
}

/// A session as the daemon has it: its log (entries, and entries of kinds
/// this build does not know as `Unknown`), and what a client attached from
/// the start saw live, deltas included.
#[derive(Default)]
struct Session {
    log: Vec<DaemonMsg>,
    live: Vec<DaemonMsg>,
    seq: u64,
}

impl Session {
    fn commit(&mut self, body: Body) -> u64 {
        self.seq += 1;
        let msg = DaemonMsg::Entry(Entry::new(Seq::new(self.seq).unwrap(), Epoch(0), body));
        self.log.push(msg.clone());
        self.live.push(msg);
        self.seq
    }

    fn generate(rng: &mut Rng) -> Self {
        let mut s = Self::default();
        for _ in 0..rng.below(24) {
            match rng.below(9) {
                0 => s.input(rng),
                1 => {
                    s.commit(Body::Assistant {
                        text: format!("a{}", rng.below(100)),
                    });
                }
                2 => s.tool(rng),
                3 => s.model(rng),
                4 => {
                    s.commit(Body::Decision {
                        by: "policy".into(),
                        call: None,
                        d: Verdict::OnStop {
                            text: "go on".into(),
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

    /// Input the user typed: taken as it arrives, or queued and, mostly,
    /// taken later.
    fn input(&mut self, rng: &mut Rng) {
        let text = format!("u{}", rng.below(100));
        let taken = rng.chance(60);
        let mode = if rng.chance(50) {
            Mode::Steer
        } else {
            Mode::Queue
        };
        let input = self.commit(Body::Input {
            segments: text.into(),
            mode,
            taken,
            origin: None,
            force: false,
        });
        if !taken && rng.chance(70) {
            self.commit(Body::Take {
                input: Seq::new(input).unwrap(),
                into: None,
            });
        }
    }

    fn tool(&mut self, rng: &mut Rng) {
        let input: Json =
            Json::encode(&json!({ "command": format!("echo {}", rng.below(10)) })).unwrap();
        let call = self.commit(Body::ToolCall {
            invoke: None,
            tool: "bash".into(),
            input,
            id: None,
            card: None,
            by: Audience::Model,
        });
        let id = CallId::new(call).unwrap();
        if rng.chance(50) {
            self.commit(Body::Decision {
                by: "policy".into(),
                call: Some(id),
                d: Verdict::Permission {
                    permission: Permission::Ask(protocol::ApprovalReason::Mode {
                        mode: "ask".into(),
                    }),
                },
            });
            if rng.chance(30) {
                return;
            }
            self.commit(Body::Answer {
                call: id,
                value: Json::encode(&rng.chance(50)).unwrap(),
                origin: None,
            });
        }
        if rng.chance(50) {
            self.commit(Body::ToolProcess {
                call: id,
                pgid: 7,
                boot: "b".into(),
                background: false,
            });
        }
        if rng.chance(80) {
            let outcome = outcome(rng, &json!({ "output": "x\n", "exit": 0 }));
            self.commit(Body::ToolResult {
                call: id,
                outcome,
                card: Some(Card {
                    title: "$ echo".into(),
                    body: "x\n".into(),
                    note: "exit 0".into(),
                    ..Card::default()
                }),
                held: Vec::new(),
            });
        }
    }

    fn model(&mut self, rng: &mut Rng) {
        let call = self.commit(Body::ModelCall {
            request: None,
            model: "m".into(),
            attempt: 0,
            choice: protocol::ToolChoice::Auto,
            r#for: None,
            purpose: None,
        });
        let call = CallId::new(call).unwrap();
        let text = format!("reply {}", rng.below(1000));
        let mut rest = text.as_str();
        while !rest.is_empty() {
            // ASCII, so any cut is a char boundary.
            let at = (1 + rng.below(4) as usize).min(rest.len());
            let (chunk, tail) = rest.split_at(at);
            self.live.push(DaemonMsg::Delta {
                call,
                text: chunk.into(),
                attempt: 0,
            });
            rest = tail;
        }
        if rng.chance(80) {
            let output = json!({
                "message": { "content": [{ "type": "text", "text": text }] },
                "usage": { "input": 3, "output": 5 },
            });
            let outcome = outcome(rng, &output);
            self.commit(Body::ModelResult {
                r#for: None,
                failure: None,
                call,
                outcome,
            });
        }
    }

    fn child(&mut self, rng: &mut Rng) {
        let child = SessionId(Uuid::from_u128(u128::from(rng.next())));
        self.commit(Body::Link {
            intent: Json::null(),
            child,
            input: "go".into(),
        });
        if rng.chance(70) {
            let watch = self.commit(Body::Watch { child });
            if rng.chance(70) {
                let outcome = outcome(rng, &json!({ "slept": 1 }));
                self.commit(Body::Fired {
                    watch: Seq::new(watch).unwrap(),
                    outcome,
                });
            }
        }
    }

    /// What an attach after `since` is sent, as the daemon sends it: the
    /// entries after it, then `Synced`.
    fn attach(&self, since: u64, upto: u64) -> Vec<DaemonMsg> {
        let mut msgs = vec![DaemonMsg::Attached {
            session: SESSION,
            workspace: "/w".into(),
        }];
        let seq = |m: &DaemonMsg| match m {
            DaemonMsg::Entry(e) => e.seq,
            DaemonMsg::Unknown { seq } => Seq::new(seq.unwrap()).unwrap(),
            _ => unreachable!(),
        };
        let log: Vec<_> = self.log.iter().filter(|m| seq(m).get() <= upto).collect();
        msgs.extend(
            log.iter()
                .filter(|m| seq(m).get() > since)
                .map(|&m| m.clone()),
        );
        msgs.push(DaemonMsg::Synced { seq: upto });
        msgs
    }

    /// [`Session::attach`] from far behind: the entries up to `upto` as one
    /// view, through its wire form, in place of their replay.
    fn attach_snapshot(&self, upto: u64) -> Vec<DaemonMsg> {
        let entries: Vec<Entry> = self
            .log
            .iter()
            .filter_map(|m| match m {
                DaemonMsg::Entry(e) if e.seq.get() <= upto => Some(e.clone()),
                _ => None,
            })
            .collect();
        let view = SessionView::from_log(SESSION, "/w".into(), &entries);
        let wire = serde_json::to_vec(&DaemonMsg::Snapshot(Box::new(view.snapshot()))).unwrap();
        let mut msgs = self.attach(upto, upto);
        msgs.insert(1, DaemonMsg::from_json(&wire).unwrap());
        msgs
    }

    /// The live stream as a client attached from the start sees it.
    fn watched(&self) -> Vec<DaemonMsg> {
        let mut msgs = self.attach(0, 0);
        msgs.extend(self.live.iter().cloned());
        msgs
    }
}

fn outcome(rng: &mut Rng, output: &Value) -> Outcome {
    match rng.below(5) {
        0 => Outcome::Failed { error: "f".into() },
        1 => Outcome::Interrupted { output: None },
        _ => Outcome::Done {
            output: Json::encode(output).unwrap().into(),
        },
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
        a.state.inbox().collect::<Vec<_>>(),
        b.state.inbox().collect::<Vec<_>>(),
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
                DaemonMsg::Entry(e) => Some(e.seq.get()),
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
fn a_snapshot_folds_to_the_replay_it_stands_for() {
    for seed in 1..=SEEDS {
        let s = Session::generate(&mut Rng(seed));
        let replayed = folded(&s.attach(0, s.seq));
        let snapshot = folded(&s.attach_snapshot(s.seq));
        assert_same(&snapshot, &replayed, seed);
    }
}

/// A client that falls behind at any point and is caught up with a
/// snapshot of any later point, then follows live again, ends where one
/// that kept up does.
#[test]
fn snapshot_plus_suffix_equals_live_view() {
    for seed in 1..=SEEDS {
        let mut rng = Rng(seed);
        let s = Session::generate(&mut rng);
        let watched = s.watched();
        let whole = folded(&watched);

        let cut = rng.below(watched.len() as u64 + 1) as usize;
        let resume = cut + rng.below((watched.len() - cut) as u64 + 1) as usize;
        let mut view = folded(&watched[..cut]);
        let upto = watched[..resume]
            .iter()
            .filter_map(|m| match m {
                DaemonMsg::Entry(e) => Some(e.seq.get()),
                DaemonMsg::Unknown { seq } => *seq,
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let updates = fold(&mut view, &s.attach_snapshot(upto));
        assert!(
            updates.iter().any(|u| matches!(u.change, Change::Snapshot)),
            "seed {seed}"
        );
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
            if matches!(msg, DaemonMsg::Entry(_)) {
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

#[test]
fn activity_never_certifies_a_newer_entry_or_a_replacement_view() {
    let mut view = SessionView::default();
    let idle = |seq| DaemonMsg::Activity {
        seq,
        activity: protocol::Activity::Idle,
    };
    view.fold(&idle(0));
    assert_eq!(view.activity, Some(protocol::Activity::Idle));
    view.fold(&DaemonMsg::Unknown { seq: Some(1) });
    assert_eq!(view.activity, None);
    assert!(view.fold(&idle(0)).is_none());
    assert_eq!(view.activity, None);
    view.fold(&idle(1));
    assert_eq!(view.activity, Some(protocol::Activity::Idle));
    let snapshot = DaemonMsg::Snapshot(Box::new(view.snapshot()));
    view.fold(&snapshot);
    assert_eq!(view.activity, None);
    view.fold(&idle(1));
    view.fold(&DaemonMsg::Attached {
        session: SESSION,
        workspace: "/w".into(),
    });
    assert_eq!(view.activity, None);
}

/// Input queued, one withdrawn and sent again edited, a background job's
/// end queued and taken up hidden, then the user's input.
fn queue_entries() -> Vec<Entry> {
    let queued = |text: &str| Body::Input {
        segments: text.into(),
        mode: Mode::Queue,
        taken: false,
        origin: None,
        force: false,
    };
    let job = protocol::Envelope::own(
        SESSION,
        CallId::new(1).unwrap(),
        protocol::EnvelopeId::JOB,
        "[background job 7 ended: exit code 0]".into(),
    );
    let seq = |n| Seq::new(n).unwrap();
    [
        Body::Input {
            segments: "work".into(),
            mode: Mode::Queue,
            taken: true,
            origin: None,
            force: false,
        },
        queued("later"),
        queued("edit me"),
        Body::Withdraw {
            input: seq(3),
            origin: None,
        },
        queued("edited"),
        Body::Enqueue { envelope: job },
        Body::Take {
            input: seq(6),
            into: None,
        },
        Body::Take {
            input: seq(2),
            into: None,
        },
    ]
    .into_iter()
    .zip(1..)
    .map(|(body, n)| Entry::new(seq(n), Epoch(0), body))
    .collect()
}

fn queue_scenario() -> Vec<Value> {
    let attached = DaemonMsg::Attached {
        session: SESSION,
        workspace: "/w".into(),
    };
    std::iter::once(attached)
        .chain(queue_entries().into_iter().map(DaemonMsg::Entry))
        .chain([DaemonMsg::Synced { seq: 8 }])
        .map(|m| serde_json::to_value(m).unwrap())
        .collect()
}

#[test]
fn a_withdrawn_input_leaves_every_clients_queue_and_hidden_items_never_join_the_transcript() {
    let entries = queue_entries();
    let queued = |upto: usize| {
        let view = SessionView::from_log(SESSION, "/w".into(), &entries[..upto]);
        let queue: Vec<_> = (view.snapshot().state.queued())
            .map(|q| (q.text.clone(), q.source.visible()))
            .collect();
        (view, queue)
    };
    let (_, queue) = queued(6);
    let shown = |text: &str| (text.to_owned(), true);
    let hidden = |text: &str| (text.to_owned(), false);
    assert_eq!(
        queue,
        [
            shown("later"),
            shown("edited"),
            hidden("[background job 7 ended: exit code 0]")
        ]
    );
    let (view, queue) = queued(8);
    assert_eq!(queue, [shown("edited")]);
    let users: Vec<_> = (view.items.iter())
        .filter_map(|item| match item {
            protocol::view::Item::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(users, ["work", "later"]);
}

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/view.json")
}

/// Named scenarios: the daemon messages a client receives, as JSON.
fn scenarios() -> Vec<(&'static str, Vec<Value>)> {
    let msg = |m: DaemonMsg| serde_json::to_value(m).unwrap();
    let entry = |seq, body| {
        msg(DaemonMsg::Entry(Entry::new(
            Seq::new(seq).unwrap(),
            Epoch(0),
            body,
        )))
    };
    let attached = || {
        msg(DaemonMsg::Attached {
            session: SESSION,
            workspace: "/w".into(),
        })
    };
    let input = |text: &str| Body::Input {
        segments: text.into(),
        mode: Mode::Queue,
        taken: true,
        origin: None,
        force: false,
    };
    let call = |seq| CallId::new(seq).unwrap();
    let ask = |seq| Body::Decision {
        by: "policy".into(),
        call: Some(call(seq)),
        d: Verdict::Permission {
            permission: Permission::Ask(protocol::ApprovalReason::Mode { mode: "ask".into() }),
        },
    };
    let echoed = || Outcome::Done {
        output: Json::encode(&json!({ "exit": 0, "output": "hi\n" }))
            .unwrap()
            .into(),
    };
    let card = || Card {
        title: "$ echo hi".into(),
        body: "hi\n".into(),
        note: "exit 0".into(),
        ..Card::default()
    };
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
    let review = |tool: &str, effective: Value| {
        vec![
        attached(),
        entry(1, Body::ToolCall { invoke: None, tool: tool.into(), input: Json::encode(&json!({"command":"original harmless input"})).unwrap(), id: None, card: Some(Card { title: "shortened title".into(), ..Card::default() }), by: Audience::Model }),
        entry(2, Body::Decision { by: "rewrite".into(), call: Some(call(1)), d: Verdict::PreTool { input: Json::encode(&effective).unwrap() } }),
        entry(3, ask(1)),
        msg(DaemonMsg::Activity { seq: 3, activity: protocol::Activity::Approval }),
        entry(4, Body::ModelCall { model: "fast".into(), attempt: 0, choice: protocol::ToolChoice::Auto, r#for: Some(call(1)), purpose: Some("approval-description".into()), request: None }),
        entry(5, Body::ModelResult { call: call(4), r#for: Some(call(1)), failure: None, outcome: Outcome::Done { output: Json::encode(&json!({"message":{"content":[{"type":"text","text":"Only lists files."}]}})).unwrap().into() } }),
        entry(6, annotation(1, "Only lists files.")),
        msg(DaemonMsg::Synced { seq: 6 }),
    ]
    };
    let mut scenarios = vec![
        ("the queue folds withdrawals and keeps what it hides out of the transcript", queue_scenario()),
        ("queued submission origins survive snapshots and ownership transfer", {
            let origin = protocol::Author { client: protocol::ClientId(Uuid::from_u128(0xc)), seq: protocol::ClientSeq::new(7).unwrap() };
            let queued = entry(1, Body::Input { segments: "same".into(), mode: Mode::Queue, taken: false, origin: Some(origin), force: false });
            let mut view = SessionView::default();
            view.fold(&DaemonMsg::from_json(queued.to_string().as_bytes()).unwrap());
            vec![attached(), msg(DaemonMsg::Snapshot(Box::new(view.snapshot()))), entry(2, Body::Take { input: Seq::new(1).unwrap(), into: None }), msg(DaemonMsg::Activity { seq: 2, activity: protocol::Activity::Idle }), msg(DaemonMsg::Synced { seq: 2 })]
        }),
        ("publication identities survive snapshots and later publications", {
            let entries: Vec<_> = ["skills", "mcp-schemas"].into_iter().zip(1..).map(|(namespace, seq)| Entry::new(
                Seq::new(seq).unwrap(), Epoch(0), Body::Context {
                    text: "instructions or schema".into(),
                    publication: Some(Publication { namespace: namespace.into(), key: Json::encode(&"loaded-publication").unwrap() }),
                },
            )).collect();
            let view = SessionView::from_log(SESSION, "/w".into(), &entries);
            vec![attached(), msg(DaemonMsg::Snapshot(Box::new(view.snapshot()))),
                entry(3, Body::Context { text: "later schema".into(), publication: Some(Publication {
                    namespace: "mcp-schemas".into(), key: Json::encode(&"later-publication").unwrap(),
                }) }),
                msg(DaemonMsg::Synced { seq: 3 }),
            ]
        }),
        ("flat inherited publications survive snapshots and later publications", {
            let spawned = inherited_publications();
            let view = SessionView::from_log(SESSION, "/w".into(), [&spawned]);
            vec![attached(), msg(DaemonMsg::Entry(spawned)), msg(DaemonMsg::Snapshot(Box::new(view.snapshot()))),
                entry(2, Body::Context { text: "later schema".into(), publication: Some(Publication { namespace: "mcp-schemas".into(), key: Json::encode("later-publication").unwrap() }) }),
                msg(DaemonMsg::Synced { seq: 2 }),
            ]
        }),
        ("owned_invocation", vec![
            attached(), entry(1, input("run")),
            entry(2, Body::ToolCall { invoke: None, tool: "owner".into(), input: Json::null(), id: None, card: None, by: Audience::Model }),
            entry(3, Body::ToolCall { invoke: Some(protocol::Invocation { parent: call(2), name: "lookup".into() }), tool: "lookup".into(), input: Json::null(), id: None, card: None, by: Audience::Model }),
            entry(4, Body::ToolResult { call: call(3), outcome: echoed(), card: None, held: Vec::new(), }),
            entry(5, Body::ToolResult { call: call(2), outcome: echoed(), card: None, held: Vec::new(), }),
        ]),

        ("approval reviews the effective multiline call", review("bash", json!({"command":"printf harmless\nrm -rf /tmp/approval-test-sentinel", "timeout":60000, "extra":"every argument"}))),
        ("approval reviews every edit replacement", review("edit", json!({"path":"/work/deep/config.ini", "edits":[{"old":"allow = false\nkeep = yes", "new":"allow = true\nkeep = yes"}, {"old":"delete = no", "new":"delete = yes"}], "extra":{"flag":true}}))),
        (
            "approval reason precedes its optional description",
            vec![
                attached(),
                entry(1, Body::ToolCall {
                    invoke: None,
                    tool: "bash".into(), input: Json::encode(&json!({"command":"ls"})).unwrap(),
                    id: None, card: None, by: Audience::Model,
                }),
                entry(2, ask(1)),
                msg(DaemonMsg::Activity { seq: 2, activity: protocol::Activity::Approval }),
                entry(3, Body::ModelCall {
                    model: "fast".into(), attempt: 0, choice: protocol::ToolChoice::Auto,
                    r#for: Some(call(1)), purpose: Some("approval-description".into()), request: None,
                }),
                entry(4, Body::ModelResult {
                    call: call(3), r#for: Some(call(1)), failure: None,
                    outcome: Outcome::Done { output: Json::encode(&json!({"message":{"content":[{"type":"text","text":"Lists files in the current directory."}]}})).unwrap().into() },
                }),
                entry(5, annotation(1, "Lists files in the current directory.")),
                msg(DaemonMsg::Synced { seq: 5 }),
            ],
        ),
        (
            "only a complete burst certifies idle after its final entry",
            vec![
                attached(),
                msg(DaemonMsg::Activity {
                    seq: 0,
                    activity: protocol::Activity::Idle,
                }),
                entry(1, input("hi")),
                msg(DaemonMsg::Activity {
                    seq: 1,
                    activity: protocol::Activity::Running,
                }),
                entry(2, Body::Assistant { text: "hi".into() }),
                msg(DaemonMsg::Activity {
                    seq: 1,
                    activity: protocol::Activity::Idle,
                }),
                msg(DaemonMsg::Activity {
                    seq: 2,
                    activity: protocol::Activity::Idle,
                }),
                msg(DaemonMsg::Synced { seq: 2 }),
            ],
        ),
        (
            "each committed tool progress chunk updates its live presentation",
            vec![
                attached(),
                entry(
                    1,
                    Body::ToolCall {
                        invoke: None,
                        tool: "bash".into(),
                        input: Json::encode(&json!({"command":"echo hi"})).unwrap(),
                        id: None,
                        card: None,
                        by: Audience::Model,
                    },
                ),
                entry(
                    2,
                    Body::Progress {
                        call: call(1),
                        text: "h".into(),
                    },
                ),
                entry(
                    3,
                    Body::Progress {
                        call: call(1),
                        text: "i\n".into(),
                    },
                ),
                entry(
                    4,
                    Body::ToolResult {
                        call: call(1),
                        outcome: echoed(),
                        card: Some(card()), held: Vec::new(),
                    },
                ),
                msg(DaemonMsg::Synced { seq: 4 }),
            ],
        ),
        (
            "a streamed reply, then the committed one",
            vec![
                attached(),
                msg(DaemonMsg::Synced { seq: 0 }),
                entry(1, input("hi")),
                entry(
                    2,
                    Body::ModelCall {
                        request: None,
                        model: "m".into(),
                        attempt: 0,
                        choice: protocol::ToolChoice::Auto,
                        r#for: None,
                        purpose: None,
                    },
                ),
                msg(DaemonMsg::Delta {
                    call: CallId::new(2).unwrap(),
                    text: "hel".into(),
                    attempt: 0,
                }),
                msg(DaemonMsg::Delta {
                    call: CallId::new(2).unwrap(),
                    text: "lo".into(),
                    attempt: 0,
                }),
                entry(
                    3,
                    Body::ModelResult {
                        r#for: None,
                        failure: None,
                        call: CallId::new(2).unwrap(),
                        outcome: Outcome::Done {
                            output: Json::encode(&json!({
                                "message": { "content": [{ "type": "text", "text": "hello" }] },
                                "usage": { "input": 4, "output": 2 },
                            }))
                            .unwrap().into(),
                        },
                    },
                ),
            ],
        ),
        (
            "an approval asked, then answered",
            vec![
                attached(),
                entry(
                    1,
                    Body::ToolCall {
                        invoke: None,
                        tool: "bash".into(),
                        input: Json::encode(&json!({ "command": "echo hi" })).unwrap(),
                        id: None,
                        card: None,
                        by: Audience::Model,
                    },
                ),
                entry(2, ask(1)),
                msg(DaemonMsg::Synced { seq: 2 }),
                entry(
                    3,
                    Body::Answer {
                        call: call(1),
                        value: Json::encode(&true).unwrap(),
                        origin: None,
                    },
                ),
                entry(
                    4,
                    Body::ToolResult {
                        call: CallId::new(1).unwrap(),
                        outcome: echoed(),
                        card: Some(card()), held: Vec::new(),
                    },
                ),
            ],
        ),
        (
            "input queued, steered and taken up",
            vec![
                attached(),
                entry(1, input("one")),
                entry(
                    2,
                    Body::Input {
                        segments: "two".into(),
                        mode: Mode::Steer,
                        taken: false,
                        origin: None,
                        force: false,
                    },
                ),
                entry(
                    3,
                    Body::Decision {
                        by: "retry".into(),
                        call: Some(call(1)),
                        d: Verdict::Retry {
                            attempt: 1,
                            error: "rate limited".into(),
                        },
                    },
                ),
                entry(
                    4,
                    Body::Take {
                        input: Seq::new(2).unwrap(),
                        into: None,
                    },
                ),
                msg(DaemonMsg::Synced { seq: 4 }),
            ],
        ),
        (
            "a child, its watch, and the watch firing",
            vec![
                attached(),
                entry(
                    1,
                    Body::Link {
                        intent: Json::null(),
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
                        watch: Seq::new(2).unwrap(),
                        outcome: Outcome::Done {
                            output: Json::encode(&json!({ "slept": 1 })).unwrap().into(),
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
                entry(1, input("one")),
                entry(2, Body::Assistant { text: "one".into() }),
                msg(DaemonMsg::Synced { seq: 2 }),
                msg(DaemonMsg::Draining),
                attached(),
                entry(2, Body::Assistant { text: "one".into() }),
                entry(3, input("two")),
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
                        invoke: None,
                        tool: "bash".into(),
                        input: Json::encode(&json!({ "command": "echo hi" })).unwrap(),
                        id: None,
                        card: Some(Card {
                            title: "$ echo hi".into(),
                            ..Card::default()
                        }),
                        by: Audience::Model,
                    },
                ),
                entry(
                    2,
                    Body::ToolResult {
                        call: CallId::new(1).unwrap(),
                        outcome: echoed(),
                        card: Some(card()), held: Vec::new(),
                    },
                ),
                entry(
                    3,
                    Body::Ext {
                        ext: "shell".into(),
                        state: "cwd".into(),
                        data: Json::encode(&json!({ "cwd": "/w/src" })).unwrap(),
                    },
                ),
                entry(4, input("/jobs")),
                entry(
                    5,
                    Body::ToolCall {
                        invoke: None,
                        tool: "jobs".into(),
                        input: Json::encode(&json!({ "args": "" })).unwrap(),
                        id: None,
                        card: None,
                        by: Audience::User,
                    },
                ),
                entry(
                    6,
                    Body::ToolResult {
                        call: CallId::new(5).unwrap(),
                        outcome: Outcome::Done {
                            output: Json::encode("no jobs").unwrap().into(),
                        },
                        card: None, held: Vec::new(),
                    },
                ),
                msg(DaemonMsg::Synced { seq: 6 }),
            ],
        ),
        ("a client that fell behind is caught up with a snapshot", {
            let asked = Body::ToolCall {
                invoke: None,
                tool: "bash".into(),
                input: Json::encode(&json!({ "command": "echo hi" })).unwrap(),
                id: None,
                card: None,
                by: Audience::Model,
            };
            let log = [
                input("one"),
                Body::ModelCall {
                    request: None,
                    model: "m".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: None,
                    purpose: None,
                },
                asked,
                ask(3),
            ]
            .into_iter()
            .zip(1..)
            .map(|(body, seq)| Entry::new(Seq::new(seq).unwrap(), Epoch(0), body))
            .collect::<Vec<_>>();
            let view = SessionView::from_log(SESSION, "/w".into(), &log);
            vec![
                attached(),
                entry(1, input("one")),
                entry(
                    2,
                    Body::ModelCall {
                        request: None,
                        model: "m".into(),
                        attempt: 0,
                        choice: protocol::ToolChoice::Auto,
                        r#for: None,
                        purpose: None,
                    },
                ),
                msg(DaemonMsg::Synced { seq: 2 }),
                msg(DaemonMsg::Delta {
                    call: CallId::new(2).unwrap(),
                    text: "thinking".into(),
                    attempt: 0,
                }),
                attached(),
                msg(DaemonMsg::Snapshot(Box::new(view.snapshot()))),
                msg(DaemonMsg::Synced { seq: 4 }),
                entry(
                    5,
                    Body::Assistant {
                        text: "done".into(),
                    },
                ),
            ]
        }),
        ("a result held as a blob shows what it is and its reference", {
            let blob = protocol::BlobRef {
                id: protocol::BlobId([0x5a; 32]),
                size: 11_000,
                media_type: "text/plain".into(),
                origin: protocol::BlobOrigin::ToolOutput(CallId::new(1).unwrap()),
            };
            vec![
                attached(),
                entry(
                    1,
                    Body::ToolCall {
                        invoke: None,
                        tool: "print".into(),
                        input: Json::encode(&json!({ "count": 1000 })).unwrap(),
                        id: None,
                        card: None,
                        by: Audience::Model,
                    },
                ),
                entry(
                    2,
                    Body::ToolResult {
                        call: CallId::new(1).unwrap(),
                        outcome: Outcome::Done {
                            output: protocol::Payload::Blob(Box::new(blob)),
                        },
                        card: None,
                        held: Vec::new(),
                    },
                ),
                msg(DaemonMsg::Synced { seq: 2 }),
            ]
        }),
        ("generated session 7", generated(7)),
        ("generated session 11", generated(11)),
    ];
    for (source, name) in [
        (
            "approval reviews the effective multiline call",
            "approval reviews a user multiline call",
        ),
        (
            "approval reviews every edit replacement",
            "approval reviews a user edit replacement",
        ),
    ] {
        let messages = scenarios
            .iter()
            .find(|(name, _)| *name == source)
            .unwrap()
            .1
            .iter()
            .map(|value| {
                let mut msg = DaemonMsg::from_json(value.to_string().as_bytes()).unwrap();
                if let DaemonMsg::Entry(entry) = &mut msg
                    && let Body::ToolCall { by, .. } = &mut entry.body
                {
                    *by = Audience::User;
                }
                serde_json::to_value(msg).unwrap()
            })
            .collect();
        scenarios.push((name, messages));
    }
    scenarios
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
                    let msg = DaemonMsg::from_json(m.to_string().as_bytes())
                        .unwrap_or_else(|error| panic!("{name}: {m}: {error}"));
                    view.fold(&msg)
                })
                .collect();
            json!({
                "name": name,
                "frames": messages.iter().map(Value::to_string).collect::<Vec<_>>(),
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

/// The recap an attach shows is read from what its replay held: the
/// user's last message, what the turn did since, and what waits.
#[test]
fn a_recap_tells_what_the_replay_held() {
    let entry = |seq, body| DaemonMsg::Entry(Entry::new(Seq::new(seq).unwrap(), Epoch(0), body));
    let mut view = SessionView::default();
    let attached = DaemonMsg::Attached {
        session: SESSION,
        workspace: "/w".into(),
    };
    let log = [
        entry(
            1,
            Body::Input {
                segments: "list files\nand more".into(),
                mode: Mode::Queue,
                taken: true,
                origin: None,
                force: false,
            },
        ),
        entry(
            2,
            Body::ToolCall {
                invoke: None,
                tool: "bash".into(),
                input: Json::encode(&json!({ "command": "ls" })).unwrap(),
                id: None,
                card: None,
                by: Audience::Model,
            },
        ),
        entry(
            3,
            Body::Decision {
                by: "policy".into(),
                call: CallId::new(2),
                d: Verdict::Permission {
                    permission: Permission::Ask(protocol::ApprovalReason::Mode {
                        mode: "ask".into(),
                    }),
                },
            },
        ),
        DaemonMsg::Synced { seq: 3 },
    ];
    fold(&mut view, std::slice::from_ref(&attached));
    fold(&mut view, &log);
    let recap = Recap {
        prompt: Some("list files…".into()),
        tools: vec!["bash".into()],
        asks: 1,
        ..Recap::default()
    };
    assert_eq!(view.recap, Some(recap));
    assert_eq!(view.state.inbox().next(), CallId::new(2));
    // Attached again with nothing new: nothing to tell.
    fold(&mut view, &[attached, DaemonMsg::Synced { seq: 3 }]);
    assert_eq!(view.recap, None);
}

#[test]
fn clients_describe_calls_and_recaps_alike() {
    use protocol::view::{call_title, decision_line, recap_line};

    let ls: Json = Json::encode(&json!({"command": "ls"})).unwrap();
    let card = Card {
        title: "$ ls -a".into(),
        body: "a\n".into(),
        note: "exit 0".into(),
        ..Card::default()
    };
    assert_eq!(call_title("bash", &ls, Some(&card)), "$ ls -a");
    assert_eq!(call_title("bash", &ls, None), "$ ls");
    assert_eq!(
        call_title("edit", &Json::encode(&json!({"path": "a"})).unwrap(), None),
        r#"edit {"path":"a"}"#
    );
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
    let blocked = Verdict::Block {
        reason: "no network".into(),
    };
    assert_eq!(
        decision_line("bash", CallId::new(2), &blocked),
        "blocked bash: no network"
    );
    let asked = Verdict::Permission {
        permission: Permission::Ask(protocol::ApprovalReason::Mode { mode: "ask".into() }),
    };
    assert_eq!(decision_line("bash", CallId::new(2), &asked), "bash: ask");
    let retried = Verdict::Retry {
        attempt: 2,
        error: "e".into(),
    };
    assert_eq!(
        decision_line("", CallId::new(3), &retried),
        "retried call 3 (attempt 2): e"
    );
}

#[test]
fn answered_and_cancelled_questions_stop_presenting_existing_descriptions() {
    for terminal in [
        Body::Answer {
            call: CallId::new(1).unwrap(),
            value: Json::encode(&true).unwrap(),
            origin: None,
        },
        Body::Answer {
            call: CallId::new(1).unwrap(),
            value: Json::encode(&false).unwrap(),
            origin: None,
        },
        Body::ToolResult {
            call: CallId::new(1).unwrap(),
            outcome: Outcome::Interrupted { output: None },
            card: None,
            held: Vec::new(),
        },
    ] {
        let (_, messages) = scenarios()
            .into_iter()
            .find(|(name, _)| *name == "approval reason precedes its optional description")
            .unwrap();
        let mut view = SessionView::default();
        for value in messages {
            view.fold(&DaemonMsg::from_json(value.to_string().as_bytes()).unwrap());
        }
        assert!(view.present(&view.items[0]).description.is_some());
        view.fold(&DaemonMsg::Entry(Entry::new(
            Seq::new(view.seq + 1).unwrap(),
            Epoch(0),
            terminal,
        )));
        assert!(view.present(&view.items[0]).description.is_none());
    }
}

#[test]
fn view_snapshots_contain_no_execution_authority() {
    for seed in 1..=SEEDS {
        let s = Session::generate(&mut Rng(seed));
        let view = folded(&s.watched());
        let mut changed = s.watched();
        for message in &mut changed {
            if let DaemonMsg::Entry(entry) = message {
                match &mut entry.body {
                    Body::Environment(env) => {
                        env.vars.insert("EXECUTION_SECRET".into(), "hidden".into());
                    }
                    Body::ToolProcess { pgid, boot, .. } => {
                        *pgid = 999;
                        *boot = "different-boot".into();
                    }
                    Body::ModelCall { attempt, .. } => *attempt = 99,
                    _ => {}
                }
            }
        }
        assert_eq!(folded(&changed).state_hash(), view.state_hash());
        let snapshot = serde_json::to_value(view.snapshot()).unwrap();
        let state = snapshot["state"].as_object().unwrap();
        assert_eq!(
            state.keys().map(String::as_str).collect::<Vec<_>>(),
            ["calls", "queued", "taken", "turn"]
        );
        for call in state["calls"].as_array().unwrap() {
            assert_eq!(
                call[1]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["phase", "progress"]
            );
        }
        assert!(snapshot.get("ledger").is_none());
    }
}

#[test]
fn view_snapshots_preserve_skill_and_mcp_publications() {
    let session = SESSION;
    let snapshot = |namespace: Option<&str>| {
        let entry = Entry::new(
            Seq::new(1).unwrap(),
            Epoch(0),
            Body::Context {
                text: "published instructions or schema".into(),
                publication: namespace.map(|namespace| Publication {
                    namespace: namespace.into(),
                    key: Json::encode(&"loaded-publication").unwrap(),
                }),
            },
        );
        let view = SessionView::from_log(session, "/w".into(), [entry]);
        let encoded = serde_json::to_vec(&DaemonMsg::Snapshot(Box::new(view.snapshot()))).unwrap();
        let mut reconnected = SessionView::default();
        reconnected.fold(&DaemonMsg::from_json(&encoded).unwrap());
        if let Some(namespace) = namespace {
            assert!(
                reconnected
                    .state
                    .publications()
                    .contains(namespace, &Json::encode(&"loaded-publication").unwrap())
            );
        }
        let snapshot = serde_json::to_value(reconnected.snapshot()).unwrap();
        assert!(
            !snapshot
                .to_string()
                .contains("published instructions or schema")
        );
        snapshot
    };
    let unpublished = snapshot(None);
    for namespace in ["skills", "mcp-schemas"] {
        assert_ne!(
            snapshot(Some(namespace)),
            unpublished,
            "reconnected clients must retain effective {namespace} publication identities"
        );
    }
}

fn inherited_publications() -> Entry {
    let key = Json::encode(&"loaded-publication").unwrap();
    let inherited = vec![protocol::publication::ContextLayer(
        protocol::publication::RunnerKind::Agent,
        ["skills", "mcp-schemas"]
            .into_iter()
            .zip(1..)
            .map(|(namespace, seq)| {
                protocol::publication::ContextInput(
                    Seq::new(seq).unwrap(),
                    protocol::publication::ContextValue::Context(
                        "execution context".into(),
                        Some(Publication {
                            namespace: namespace.into(),
                            key: key.clone(),
                        }),
                    ),
                )
            })
            .collect(),
    )];
    Entry::new(
        Seq::new(1).unwrap(),
        Epoch(0),
        Body::Spawned {
            intent: protocol::publication::SpawnImage {
                runner: protocol::publication::RunnerKind::Agent,
                authority: Json::encode(&"execution authority").unwrap(),
                image: inherited,
            }
            .to_json()
            .unwrap(),
        },
    )
}

#[test]
fn view_publications_follow_edits_to_inherited_context() {
    let key = Json::encode(&"loaded-publication").unwrap();
    let spawned = inherited_publications();
    let view = SessionView::from_log(SESSION, "/w".into(), [spawned]);
    let encoded = serde_json::to_vec(&DaemonMsg::Snapshot(Box::new(view.snapshot()))).unwrap();
    assert!(!String::from_utf8_lossy(&encoded).contains("execution"));
    let mut restored = SessionView::default();
    restored.fold(&DaemonMsg::from_json(&encoded).unwrap());
    for namespace in ["skills", "mcp-schemas"] {
        assert!(restored.state.publications().contains(namespace, &key));
    }
    restored.fold(&DaemonMsg::Entry(Entry::new(
        Seq::new(2).unwrap(),
        Epoch(0),
        Body::Edit {
            target: Seq::new(1).unwrap(),
            upto: None,
            replace: None,
        },
    )));
    assert!(restored.state.publications().is_empty());
}

fn annotation(target: u64, body: &str) -> Body {
    Body::Fact {
        fact: protocol::ExtensionFact::Annotation(protocol::Annotation {
            target: CallId::new(target).unwrap(),
            lifetime: protocol::AnnotationLifetime::Question,
            card: Card {
                body: body.into(),
                ..Card::default()
            },
        }),
    }
}

#[test]
fn extension_presentation_needs_no_purpose_string_branch() {
    let (_, messages) = scenarios()
        .into_iter()
        .find(|(name, _)| *name == "approval reason precedes its optional description")
        .unwrap();
    let mut view = SessionView::default();
    for message in messages {
        let mut message = DaemonMsg::from_json(message.to_string().as_bytes()).unwrap();
        if let DaemonMsg::Entry(entry) = &mut message
            && let Body::ModelCall { purpose, .. } = &mut entry.body
        {
            *purpose = Some("an arbitrary extension purpose".into());
        }
        view.fold(&message);
    }
    assert_eq!(
        view.present(&view.items[0]).description,
        Some("Lists files in the current directory.")
    );
    let snapshot = view.snapshot();
    let mut attached = SessionView::default();
    attached.fold(&DaemonMsg::Snapshot(Box::new(snapshot)));
    assert_eq!(
        attached.present(&attached.items[0]),
        view.present(&view.items[0])
    );
}

#[test]
fn call_annotations_present_generic_cards_until_the_call_finishes() {
    let call = CallId::new(1).unwrap();
    let mut view = SessionView::default();
    for (seq, body) in [
        Body::ModelCall {
            model: "fast".into(),
            attempt: 0,
            choice: protocol::ToolChoice::Auto,
            r#for: None,
            purpose: Some("custom".into()),
            request: None,
        },
        Body::Fact {
            fact: protocol::ExtensionFact::Annotation(protocol::Annotation {
                target: call,
                lifetime: protocol::AnnotationLifetime::Call,
                card: Card {
                    title: "Extension status".into(),
                    body: "Typed presentation".into(),
                    note: "working".into(),
                    mark: protocol::Mark::Warn,
                },
            }),
        },
    ]
    .into_iter()
    .enumerate()
    {
        view.fold(&DaemonMsg::Entry(Entry::new(
            Seq::new(seq as u64 + 1).unwrap(),
            Epoch(0),
            body,
        )));
    }
    let shown = view.present(&view.items[0]);
    assert_eq!(shown.title, "Extension status");
    assert_eq!(shown.body.text(), "Typed presentation");
    assert_eq!(shown.status, "working");
    assert_eq!(shown.glyph, protocol::view::Glyph::Warn);
    view.fold(&DaemonMsg::Entry(Entry::new(
        Seq::new(3).unwrap(),
        Epoch(0),
        Body::ModelResult {
            call,
            r#for: None,
            failure: None,
            outcome: Outcome::Interrupted { output: None },
        },
    )));
    assert_eq!(view.present(&view.items[0]).title, "custom");
}

/// A client never has a blob's bytes: it shows what the result is and the
/// reference the daemon serves it by.
#[test]
fn a_result_held_as_a_blob_is_shown_as_its_reference() {
    let (_, messages) = scenarios()
        .into_iter()
        .find(|(name, _)| name.starts_with("a result held as a blob"))
        .unwrap();
    let mut view = SessionView::default();
    for value in messages {
        view.fold(&DaemonMsg::from_json(value.to_string().as_bytes()).unwrap());
    }
    let shown = view.present(&view.items[0]);
    let Shown::Held(blob) = shown.body else {
        panic!("{shown:?}");
    };
    assert_eq!(blob.size, 11_000);
    assert_eq!(
        shown.body.text(),
        format!(
            "11000 bytes of text/plain, held as blob {}",
            "5a".repeat(32)
        )
    );
}
