use std::path::{Path, PathBuf};

use protocol::{DaemonMsg, Decode, Json, Permission};

use super::*;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The entries of every recorded sequence of daemon messages.
#[test]
fn the_ledger_reads_the_view_vectors_as_the_scans_do() {
    let path = root().join("crates/protocol/tests/vectors/view.json");
    let vectors: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    for vector in vectors {
        let entries: Vec<Entry> = vector["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(
                |m| match DaemonMsg::from_json(&serde_json::to_vec(m).unwrap()).ok()? {
                    DaemonMsg::Entry(e) => Some(e),
                    _ => None,
                },
            )
            .collect();
        check_each(&entries);
    }
}

/// Session logs named by `A_ORACLE_LOGS` (colon-separated), such as long
/// real ones: checked at their end and at a sample of prefixes.
#[test]
fn the_ledger_reads_logs_named_as_the_scans_do() {
    let Ok(paths) = std::env::var("A_ORACLE_LOGS") else {
        return;
    };
    for path in paths.split(':') {
        let bytes = std::fs::read(path).unwrap();
        let entries: Vec<Entry> = bytes
            .split(|&b| b == b'\n')
            .skip(1)
            .filter(|line| !line.is_empty())
            .map(|line| Entry::from_json(line).unwrap())
            .collect();
        let mut ledger = Ledger::default();
        let every = (entries.len() / 16).max(1);
        for (at, e) in entries.iter().enumerate() {
            ledger.apply(e);
            if at % every == 0 || at + 1 == entries.len() {
                check(&entries[..=at], &ledger);
            }
        }
    }
}

/// A small deterministic generator (xorshift), so a failure repeats.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> Option<T> {
        (!from.is_empty()).then(|| from[self.below(from.len())])
    }

    fn outcome(&mut self) -> Outcome {
        match self.below(3) {
            0 => Outcome::Done {
                output: Json::from(self.below(5)),
            },
            1 => Outcome::Failed { error: "e".into() },
            _ => Outcome::Interrupted {
                output: (self.below(2) == 0).then(|| "o".into()),
            },
        }
    }
}

/// A log as the engine could write it: outcomes only for open calls, asks
/// only for open tool calls not asked yet, answers only for asks, steers
/// only for queued input, fires only for armed watches.
#[allow(clippy::too_many_lines)]
fn log(rng: &mut Rng, len: usize) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let children: Vec<SessionId> = (0..3).map(|_| SessionId::new()).collect();
    for seq in 1..=len as u64 {
        let ledger = Ledger::of(&entries);
        // A call waiting for its answer settles only once answered.
        let open: Vec<u64> = ledger
            .open_calls()
            .filter(|(_, o)| o.phase != Phase::Asked)
            .map(|(s, _)| s)
            .collect();
        let calls: Vec<u64> = ledger
            .open_calls()
            .filter(|(_, o)| matches!(o.call, Call::Tool { .. }) && o.phase == Phase::Called)
            .map(|(s, _)| s)
            .collect();
        let queued: Vec<u64> = ledger.untaken().iter().map(|q| q.seq).collect();
        let commands: Vec<u64> = ledger.open_commands().collect();
        let armed: Vec<u64> = ledger.armed().map(|(s, _)| s).collect();
        let text = format!("t{}", rng.below(4));
        let body = match rng.below(24) {
            0 | 1 => Body::User { text },
            2 => Body::Queued {
                text,
                steer: rng.below(2) == 0,
            },
            3 => match rng.pick(&queued) {
                Some(queued) => Body::Steer { text, queued },
                None => Body::Interrupt,
            },
            4 => Body::Assistant { text },
            5 | 6 => Body::ToolCall {
                tool: ["bash", "sleep"][rng.below(2)].into(),
                input: Json::from(rng.below(3)),
                id: None,
                card: None,
            },
            7 => Body::ModelCall { model: "m".into() },
            8 | 9 => match rng.pick(&open) {
                Some(call) if rng.below(2) == 0 => Body::ToolResult {
                    call,
                    outcome: rng.outcome(),
                    card: None,
                },
                Some(call) => Body::ModelResult {
                    call,
                    outcome: rng.outcome(),
                },
                None => Body::Interrupt,
            },
            10 => match rng.pick(&open) {
                Some(call) => Body::ToolProcess {
                    call,
                    pgid: u32::try_from(rng.below(100)).unwrap(),
                    boot: ["b1", "b2"][rng.below(2)].into(),
                    background: rng.below(2) == 0,
                },
                None => Body::Interrupt,
            },
            11 => match rng.pick(&open) {
                Some(call) => Body::Progress { call, text },
                None => Body::Interrupt,
            },
            12 => match rng.pick(&calls) {
                Some(call) => Body::Ask { call },
                None => Body::Interrupt,
            },
            13 | 14 => {
                let decision = match rng.below(4) {
                    0 => match ledger.inbox().first() {
                        Some(&call) => Decision::Approval {
                            tool: "bash".into(),
                            approved: rng.below(2) == 0,
                            call: Some(call),
                        },
                        None => Decision::OnStop { text },
                    },
                    1 => Decision::Permission {
                        tool: "bash".into(),
                        permission: Permission::Ask,
                    },
                    2 => Decision::Approval {
                        tool: "bash".into(),
                        approved: true,
                        call: None,
                    },
                    _ => Decision::OnStop { text },
                };
                Body::Hook {
                    hook: ["user", "policy"][rng.below(2)].into(),
                    decision,
                }
            }
            15 => Body::Link {
                child: children[rng.below(children.len())],
                input: text,
            },
            16 => Body::Watch {
                child: children[rng.below(children.len())],
            },
            17 => match rng.pick(&armed) {
                Some(watch) => Body::Fired {
                    watch,
                    outcome: rng.outcome(),
                },
                None => Body::Interrupt,
            },
            18 => Body::Environment(Environment {
                cwd: format!("/{}", rng.below(3)).into(),
                ..Environment::default()
            }),
            19 => Body::Extensions {
                installed: vec![Installed {
                    name: "x".into(),
                    version: u32::try_from(rng.below(3)).unwrap(),
                }],
            },
            20 => Body::Context { text },
            21 => Body::Command {
                name: "c".into(),
                args: String::new(),
            },
            22 => match rng.pick(&commands) {
                Some(call) => Body::CommandResult {
                    call,
                    outcome: rng.outcome(),
                },
                None => Body::Interrupt,
            },
            _ => Body::Settings(Overrides {
                model: (rng.below(2) == 0).then(|| text.clone()),
            }),
        };
        entries.push(Entry::new(seq, 0, body));
    }
    entries
}

#[test]
fn the_ledger_reads_generated_logs_as_the_scans_do() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..300 {
        let len = 1 + rng.below(80);
        check_each(&log(&mut rng, len));
    }
}

/// Logs that break the engine's rules (outcomes for calls never made,
/// asks for model calls, answers before asks) fold without a panic.
#[test]
fn any_log_folds() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..300 {
        let mut ledger = Ledger::default();
        for seq in 1..=rng.below(60) as u64 {
            let call = rng.below(8) as u64;
            let body = match rng.below(8) {
                0 => Body::Ask { call },
                1 => Body::ToolResult {
                    call,
                    outcome: rng.outcome(),
                    card: None,
                },
                2 => Body::Hook {
                    hook: "user".into(),
                    decision: Decision::Approval {
                        tool: "bash".into(),
                        approved: false,
                        call: Some(call),
                    },
                },
                3 => Body::Fired {
                    watch: call,
                    outcome: rng.outcome(),
                },
                4 => Body::Steer {
                    text: "s".into(),
                    queued: call,
                },
                5 => Body::ModelCall { model: "m".into() },
                6 => Body::CommandResult {
                    call,
                    outcome: rng.outcome(),
                },
                _ => Body::Progress {
                    call,
                    text: "p".into(),
                },
            };
            ledger.apply(&Entry::new(seq, 0, body));
        }
        assert!(ledger.inbox().iter().all(|&c| ledger.asked(c).is_some()));
    }
}

/// Where the scans disagreed among themselves, the ledger has one answer:
/// a call that settled without one is no longer asked about.
#[test]
fn a_call_settled_unanswered_leaves_the_inbox() {
    let entries = [
        Entry::new(
            1,
            0,
            Body::ToolCall {
                tool: "bash".into(),
                input: Json::null(),
                id: None,
                card: None,
            },
        ),
        Entry::new(2, 0, Body::Ask { call: 1 }),
        Entry::new(
            3,
            0,
            Body::ToolResult {
                call: 1,
                outcome: Outcome::Interrupted { output: None },
                card: None,
            },
        ),
    ];
    assert_eq!(inbox(&entries), [1]);
    assert_eq!(Ledger::of(&entries).inbox(), [0; 0]);
}
