//! What holds of the kernel by construction, each kept by a test named
//! after it.

mod common;

use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::*;
use kernel::{Event, Load, Note, Owner, Replied, Turn};
use protocol::ledger::Call;
use protocol::{Body, CallId, Entry, Json, Outcome, WaitOn};
use serde_json::json;

/// Carries effects out as a driver would, at once and always the same way:
/// a tool is done with its name, a model's first reply of a turn calls
/// `sleep` and the next calls nothing, a backoff is over, an answer comes.
fn drive(s: &mut Session, fx: impl IntoIterator<Item = Work>) {
    let mut todo: VecDeque<Work> = fx.into_iter().collect();
    while let Some(effect) = todo.pop_front() {
        let event = match effect {
            Work::RunTool { call, tool, .. } | Work::Resume { call, tool, .. } => Event::ToolDone {
                call,
                outcome: done(json!(tool)),
                card: None,
                held: Vec::new(),
            },
            Work::Prepare { call, .. } => {
                let before = &s.log[usize::try_from(call.get()).unwrap() - 2].body;
                let first = matches!(before, Body::Input { .. } | Body::Take { .. });
                let calls = if first {
                    vec![("sleep", json!({ "seconds": 1 }))]
                } else {
                    vec![]
                };
                Event::ModelDone {
                    call,
                    replied: Replied::Done(reply(&calls)),
                }
            }
            Work::Sleep { call, .. } => Event::Woke { call },
            _ => continue,
        };
        todo.extend(into_work(s.step(event)));
    }
}

/// The log as the conversation it holds: what the user said, the replies
/// and the calls, and how each call ended. Retries, intents of model calls
/// and how input waited are left out.
fn conversation(log: &[Entry]) -> Vec<String> {
    let text_of = |seq: u64| match &log[usize::try_from(seq).unwrap() - 1].body {
        Body::Input { segments: text, .. } => text.clone(),
        _ => unreachable!(),
    };
    log.iter()
        .filter_map(|e| match &e.body {
            Body::Input {
                segments: text,
                taken: true,
                ..
            } => Some(format!("user {text}")),
            Body::Take { input, .. } => Some(format!("user {}", text_of(input.get()))),
            Body::Assistant { text } => Some(format!("said {text}")),
            Body::ToolCall { tool, input, .. } => Some(format!("call {tool} {input}")),
            Body::ToolResult { outcome, .. } => Some(format!("result {outcome:?}")),
            Body::ModelResult {
                outcome:
                    Outcome::Done {
                        output: protocol::Payload::Inline(output),
                    },
                ..
            } => Some(format!("reply {output}")),
            _ => None,
        })
        .collect()
}

/// Runs `inputs` to the end, all given at once, with a crash after each
/// entry of an uninterrupted run: the session resumed from what committed,
/// and given the input that was not, ends with the same conversation.
fn crash_everywhere(reg: impl Fn() -> Reg, inputs: &[&str]) {
    let mut s = Session::new(reg().arc());
    for text in inputs {
        let fx = s.step(user(text));
        drive(&mut s, into_work(fx));
    }
    let whole = s.log.clone();
    assert_eq!(*s.core.turn(), Turn::Idle, "{whole:#?}");
    for cut in 0..=whole.len() {
        let (mut s, fx) = Session::resume(reg().arc(), whole[..cut].to_vec(), &Load::default());
        drive(&mut s, into_work(fx));
        let given = whole[..cut]
            .iter()
            .filter(|e| matches!(e.body, Body::Input { .. }))
            .count();
        for text in &inputs[given..] {
            let fx = s.step(user(text));
            drive(&mut s, into_work(fx));
        }
        assert_eq!(
            conversation(&s.log),
            conversation(&whole),
            "cut after {cut}: {:#?}",
            s.log
        );
        let calls = s
            .log
            .iter()
            .filter(|e| matches!(e.body, Body::ToolCall { .. }))
            .count();
        let results = s
            .log
            .iter()
            .filter(|e| matches!(e.body, Body::ToolResult { .. }))
            .count();
        assert_eq!(calls, results, "every call answered once, cut after {cut}");
        assert_eq!(*s.core.turn(), Turn::Idle);
    }
}

/// #8: whatever entry a crash comes after, resuming the log is all the
/// recovery there is, and the session goes on as if there had been none
/// (its calls being safe to run again).
#[test]
fn recovery_is_just_resume() {
    crash_everywhere(Reg::echo, &["sleep 1", "hi", "sleep 2", "bye"]);
    crash_everywhere(|| Reg::modeled().retry(Duration::ZERO), &["go", "again"]);
}

/// Runs `inputs` as `go` carries effects out, each given once the one
/// before it is done, or with the next if it is `+`-marked; then, from
/// every point it rested at, holds that a session restored from a
/// snapshot taken there and handed the log after it resumes as one
/// handed the whole log, whatever entry a crash came after: the same
/// work, turn and ledger, and from there the same log.
fn snapshot_everywhere(
    reg: &dyn Fn() -> Reg,
    inputs: &[&str],
    go: &dyn Fn(&mut Session, Vec<Work>),
) {
    let ledger = |s: &Session| serde_json::to_value(s.core.ledger()).unwrap();
    let mut s = Session::new(reg().arc());
    let mut rests = Vec::new();
    let mut pending = Vec::new();
    for text in inputs {
        if s.core.at_rest() {
            rests.push((s.log.len(), s.core.ledger().clone()));
        }
        if let Some(text) = text.strip_prefix('+') {
            pending.extend(into_work(s.step(user(text))));
        } else {
            let mut fx = std::mem::take(&mut pending);
            fx.extend(into_work(s.step(user(text))));
            go(&mut s, fx);
        }
    }
    go(&mut s, pending);
    assert!(s.core.at_rest(), "{:#?}", s.log);
    rests.push((s.log.len(), s.core.ledger().clone()));
    let whole = s.log.clone();
    for (at, snapshot) in &rests {
        for cut in *at..=whole.len() {
            let log = whole[..cut].to_vec();
            let (mut a, fx) = Session::resume(reg().arc(), log.clone(), &Load::default());
            let (mut b, restored) =
                Session::restore(reg().arc(), snapshot, log, *at, &Load::default());
            let at = format!("snapshot at {at}, cut after {cut}");
            assert_eq!(restored, fx, "{at}");
            assert_eq!(b.core.turn(), a.core.turn(), "{at}");
            assert_eq!(ledger(&b), ledger(&a), "{at}");
            go(&mut a, into_work(fx).collect());
            go(&mut b, into_work(restored).collect());
            let given = whole[..cut]
                .iter()
                .filter(|e| matches!(e.body, Body::Input { .. }))
                .count();
            for text in &inputs[given.min(inputs.len())..] {
                let text = text.trim_start_matches('+');
                for s in [&mut a, &mut b] {
                    let fx = s.step(user(text));
                    go(s, into_work(fx).collect());
                }
            }
            assert_eq!(b.log, a.log, "{at}");
        }
    }
}

/// A snapshot is taken only where the session rests, and keeps only its
/// ledger: restoring one and folding the log after it is resuming the
/// whole log, for the echo and modeled agents and the default loop, on
/// scripted runs and generated ones.
#[test]
fn resume_from_a_snapshot_equals_resume_from_the_log() {
    let echo = &|s: &mut Session, fx| drive(s, fx);
    snapshot_everywhere(&Reg::echo, &["sleep 1", "hi", "+sleep 2", "bye"], echo);
    snapshot_everywhere(
        &|| Reg::modeled().retry(Duration::ZERO),
        &["go", "+again", "/jobs", "more"],
        echo,
    );
    let words = [
        "hi",
        "sleep 1",
        "$ ls",
        "refuse bash",
        "/jobs",
        "+hi",
        "+sleep 2",
    ];
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..12 {
        let inputs: Vec<&str> = (0..6)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                words[usize::try_from(seed % words.len() as u64).unwrap()]
            })
            .collect();
        snapshot_everywhere(&Reg::echo, &inputs, echo);
    }
    for script in [1u64, 7, 12] {
        let script = script.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let reg = move || {
            Reg::looping(|r| r.with_in(ext::Tier::Pre, &ext::StepLimit { max: 2 }))
                .retry(Duration::ZERO)
        };
        let go = move |s: &mut Session, fx| drive_loop(s, fx, script);
        snapshot_everywhere(&reg, &["a", "+b", "c"], &go);
    }
}

/// Refuses every call of a reply but its first: a `reply` handler.
struct OneAtATime;

impl ext::Extension for OneAtATime {
    const NAME: &'static str = "one";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut ext::Registrar) {
        r.on::<ext::point::Reply>(|(), replied| {
            let mut actions = replied.actions.clone();
            for action in actions.iter_mut().skip(1) {
                if let kernel::Action::Call { tool, input, id } = action {
                    *action = kernel::Action::Refuse {
                        tool: tool.clone(),
                        input: input.clone(),
                        id: id.clone(),
                        why: "one at a time".into(),
                    };
                }
            }
            ext::Rewrite::Replace(actions)
        });
    }
}

/// Carries out effects for the default loop: the session's `k`th reply
/// calls `sleep` as many times as `script` says for `k`, so a run is a
/// function of the conversation, however a crash renumbered its entries.
fn drive_loop(s: &mut Session, fx: impl IntoIterator<Item = Work>, script: u64) {
    let mut todo: VecDeque<Work> = fx.into_iter().collect();
    while let Some(effect) = todo.pop_front() {
        let event = match effect {
            Work::RunTool { call, tool, .. } => tool_done(call.get(), done(json!(tool))),
            Work::Prepare { call, .. } => {
                let k = (s.log.iter())
                    .filter(|e| matches!(e.body, Body::ModelResult { .. }))
                    .count() as u64;
                let n = ((k + 1).wrapping_mul(script) >> 7) % 3;
                let calls: Vec<_> = (0..n).map(|s| ("sleep", json!({ "seconds": s }))).collect();
                Event::ModelDone {
                    call,
                    replied: Replied::Done(response(&calls)),
                }
            }
            Work::Sleep { call, .. } => Event::Woke { call },
            _ => continue,
        };
        todo.extend(into_work(s.step(event)));
    }
}

/// The default loop is a fold, and so are its points: for scripts of
/// replies, step limits and a `reply` handler, a log cut anywhere and
/// resumed asks for what the live run asked, so it ends as the live run
/// did; and the whole log resumed asks for nothing more.
#[test]
fn loop_points_are_folds() {
    let reg = |script: u64| {
        let max = u16::try_from(script % 4).unwrap() + 1;
        Reg::looping(move |r| {
            r.with_in(ext::Tier::Pre, &ext::StepLimit { max })
                .and_then(|r| r.with(&OneAtATime))
        })
        .retry(Duration::ZERO)
    };
    let inputs = ["a", "b"];
    for script in 1..24u64 {
        let script = script.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut s = Session::new(reg(script).arc());
        for text in inputs {
            let fx = s.step(user(text));
            drive_loop(&mut s, into_work(fx), script);
        }
        let whole = s.log.clone();
        let (_, fx) = Session::resume(reg(script).arc(), whole.clone(), &Load::default());
        assert!(fx.appends.is_empty(), "{fx:?}");
        for cut in 0..=whole.len() {
            let log = whole[..cut].to_vec();
            let (mut s, fx) = Session::resume(reg(script).arc(), log, &Load::default());
            drive_loop(&mut s, into_work(fx), script);
            let given = whole[..cut]
                .iter()
                .filter(|e| matches!(e.body, Body::Input { .. }))
                .count();
            for text in &inputs[given..] {
                let fx = s.step(user(text));
                drive_loop(&mut s, into_work(fx), script);
            }
            assert_eq!(
                conversation(&s.log),
                conversation(&whole),
                "script {script}, cut after {cut}"
            );
        }
    }
}

/// A session in each state a turn can be in, with what it logged last.
fn states() -> Vec<(&'static str, Session)> {
    let mut states = Vec::new();
    let mut state = |name, reg: Reg, events: Vec<Event>| {
        let mut s = Session::new(reg.arc());
        for event in events {
            s.step(event);
        }
        states.push((name, s));
    };
    state("idle", Reg::echo(), vec![user("hi")]);
    state("model streaming", Reg::modeled(), vec![user("hi")]);
    let retrying = Reg::modeled().retry(Duration::from_secs(1));
    state("model backoff", retrying, vec![user("hi"), model_failed(2)]);
    let recovering = Reg::modeled().resend();
    state(
        "model recovering",
        recovering,
        vec![user("hi"), model_failed(2)],
    );
    state("tool running", Reg::echo(), vec![user("sleep 1")]);
    state(
        "gate awaiting",
        Reg::echo().policy(Policy::Later),
        vec![user("$ x")],
    );
    let asking = Reg::echo().policy(Policy::Ask(&["bash"]));
    state("waiting approval", asking, vec![user("$ x")]);
    let dialog = Event::Waits {
        call: id(2),
        on: WaitOn::Dialog { ui: Json::null() },
    };
    state("waiting dialog", Reg::echo(), vec![user("$ x"), dialog]);
    let spawn = Event::Spawn {
        intent: Json::null(),
        call: id(2),
        child: child(),
        input: "go".into(),
        wait: true,
    };
    state("waiting child", Reg::echo(), vec![user("task go"), spawn]);
    state("stopping", Reg::echo(), vec![user("sleep 1"), interrupt()]);
    state(
        "queued behind",
        Reg::echo(),
        vec![user("sleep 1"), user("sleep 2")],
    );
    states
}

fn starts_work(effect: &Work, open_at_interrupt: u64) -> bool {
    match effect {
        Work::RunTool { call, .. }
        | Work::Resume { call, .. }
        | Work::Prepare { call, .. }
        | Work::Await { call, .. }
        | Work::Recover { call, .. } => call.get() <= open_at_interrupt,
        Work::StartChild { .. } => true,
        _ => false,
    }
}

/// #10 (B1): in no state does an interrupt start work for what was open
/// when it landed, nor while what it cancelled reports; with nothing
/// running it does nothing at all.
#[test]
fn an_interrupt_never_starts_work() {
    for (name, mut s) in states() {
        let mark = s.log.len() as u64;
        let mut todo: VecDeque<Work> = into_work(s.step(interrupt())).collect();
        if name == "stopping" {
            // What the first interrupt cancelled reports now.
            todo.push_back(cancel(2));
        }
        if name == "idle" {
            assert!(todo.is_empty(), "{todo:?}");
            continue;
        }
        while let Some(effect) = todo.pop_front() {
            assert!(!starts_work(&effect, mark), "{name}: {effect:?}");
            let event = match effect {
                Work::Cancel { call } => match &s.core.ledger().call(call).map(|o| &o.call) {
                    Some(Call::Model { .. }) => Event::ModelDone {
                        call,
                        replied: Replied::Interrupted(None),
                    },
                    Some(Call::Tool { .. }) => tool_done(call.get(), INTERRUPTED),
                    Some(Call::Side { .. }) | None => continue,
                },
                Work::Interrupt { child } => Event::ChildSettled {
                    child,
                    outcome: INTERRUPTED,
                },
                _ => continue,
            };
            todo.extend(into_work(s.step(event)));
        }
        let interrupted = s.log.iter().filter(|e| e.body == Body::INTERRUPT).count();
        assert_eq!(interrupted, 1, "{name}: {:#?}", s.log);
        if name != "queued behind" {
            assert_eq!(*s.core.turn(), Turn::Idle, "{name}");
        }
    }
}

/// #11 (B2): an interrupt stops the turn it lands in, and only it.
#[test]
fn an_interrupt_stops_only_its_turn() {
    let mut s = Session::new(Reg::echo().arc());
    s.step(user("sleep 1"));
    s.step(tool_done(2, done(json!(1))));
    // Lands between turns: nothing to stop, and nothing kept.
    assert_eq!(s.step(interrupt()), StepPlan::default());
    let fx = s.step(user("sleep 2"));
    assert!(
        work(&fx).any(|e| matches!(e, Work::RunTool { .. })),
        "{fx:?}"
    );
}

/// #66: the kernel holds no clock, randomness or hash order, so the same
/// events give the same log; and a log resumed gives the state it was
/// written in (#5: replay equals live).
#[test]
fn the_same_events_give_the_same_log() {
    // The open call of the turn, whatever its number.
    let open = |s: &Session| match s.core.turn() {
        Turn::Acting { open } => open[0].0,
        turn => panic!("no call open: {turn:?}"),
    };
    let run = || {
        let mut s = Session::new(Reg::echo().on_stop("more").arc());
        s.step(user("sleep 1"));
        s.step(user("queued"));
        let note = Note::Progress("half\n".into());
        s.step(Event::Note {
            call: open(&s),
            note,
        });
        s.step(tool_done(open(&s).get(), done(json!(1))));
        s.step(user("/jobs"));
        s.step(tool_done(open(&s).get(), done(json!([]))));
        s.step(user("task go"));
        let (call, input) = (open(&s), "go".into());
        s.step(Event::Spawn {
            intent: Json::null(),
            call,
            child: child(),
            input,
            wait: true,
        });
        s.step(Event::ChildSettled {
            child: child(),
            outcome: done(json!("ok")),
        });
        s.step(tool_done(call.get(), done(json!("ok"))));
        s
    };
    let (a, b) = (run(), run());
    assert_eq!(a.log, b.log);
    assert_eq!(*a.core.turn(), Turn::Idle, "{:#?}", a.log);
    // A resume asks `on_stop` of nothing: the log shows the agent stopped.
    let reg = Reg::echo().on_stop("more");
    let (resumed, fx) = Session::resume(reg.arc(), a.log.clone(), &Load::default());
    assert!(
        (fx.appends.is_empty() && work(&fx).next().is_none()),
        "{fx:?}"
    );
    assert_eq!(resumed.core.turn(), a.core.turn());
    assert_eq!(resumed.core.ledger().activity(), a.core.ledger().activity());
    // So do the calls of one reply, run together and ending out of order.
    let together = || {
        let mut s = Session::new(Reg::modeled().arc());
        s.step(user("go"));
        let sleep = |n: u64| ("sleep", json!({ "seconds": n }));
        s.step(model_done(2, &[sleep(1), sleep(2)]));
        s.step(tool_done(5, done(json!(2))));
        s.step(tool_done(4, done(json!(1))));
        s.log
    };
    assert_eq!(together(), together());
}

/// Whether `f` panics, run as a session actor runs: in a process of its
/// own here, which takes its state with it. (Dev builds' codegen cannot
/// unwind this crate's frames, so a panic aborts, as it would a slot.)
#[allow(clippy::disallowed_methods)]
fn panics(name: &str, f: impl FnOnce()) -> bool {
    const IN_CHILD: &str = "KERNEL_TEST_PANIC";
    if std::env::var_os(IN_CHILD).is_some_and(|n| n == name) {
        f();
        std::process::exit(0);
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(IN_CHILD, name)
        .output()
        .unwrap()
        .status;
    !status.success()
}

/// #63: a panic in a step loses only what it had not handed the driver;
/// the session is resumed from what committed, on a fixed registry.
#[test]
fn a_kernel_panic_is_a_resume() {
    let panicking = || {
        let mut s = Session::new(Reg::echo().policy(Policy::Panic).arc());
        s.step(user("hi"));
        s
    };
    let committed = panicking().log;
    assert!(panics("a_kernel_panic_is_a_resume", || {
        panicking().step(user("$ ls"));
    }));
    let (mut s, fx) = Session::resume(Reg::echo().arc(), committed, &Load::default());
    assert!(
        (fx.appends.is_empty() && work(&fx).next().is_none()),
        "{fx:?}"
    );
    s.table(vec![(
        user("$ ls"),
        (
            vec![entry(3, input("$ ls")), entry(4, bash("ls"))],
            vec![BUSY, run_bash(4, "ls")],
        ),
    )]);
    let (_, fx) = Session::resume(Reg::echo().arc(), s.log, &Load::default());
    assert_plan(&fx, (vec![entry(5, tool_result(4, INTERRUPTED))], vec![]));
}

/// No extension is known by name below the extension layer: the
/// protocol, the kernel and the engine name no plugin, its state, fold or
/// purpose. (What the `settings` extension chooses is a service; what
/// titles a session is a side request's purpose, the plugin's own.) The
/// names are the `ext-*` crates'; a `kinds!` row naming a wire tag of the
/// same spelling (the home `settings` message) is no plugin.
#[test]
fn no_plugin_is_named_below_the_plugin_layer() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut plugins: Vec<String> = std::fs::read_dir(&crates)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("ext-")
                .filter(|n| *n != "derive")
                .map(|n| format!("\"{n}\""))
        })
        .collect();
    plugins.sort();
    assert!(plugins.contains(&"\"settings\"".to_owned()), "{plugins:?}");
    let wire_tag = |line: &str, literal: &str| {
        line.trim_start()
            .split_once(' ')
            .is_some_and(|(kind, rest)| {
                kind.starts_with(char::is_uppercase) && rest.starts_with(literal)
            })
    };
    let mut sources = Vec::new();
    for krate in ["protocol", "kernel", "engine"] {
        sources.extend(rust_sources(&crates.join(krate).join("src")));
    }
    assert!(sources.len() > 20, "{} sources read", sources.len());
    let mut found = Vec::new();
    for path in sources {
        let text = std::fs::read_to_string(&path).unwrap();
        // A file's own unit tests may name what they like.
        let code = text.split("#[cfg(test)]").next().unwrap_or_default();
        for (n, line) in code.lines().enumerate() {
            for plugin in &plugins {
                if line.contains(plugin.as_str()) && !wire_tag(line, plugin) {
                    found.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "a plugin named below the plugin layer:\n{}",
        found.join("\n")
    );
}

/// The non-test Rust sources under `dir`.
fn rust_sources(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name != "tests" {
                found.extend(rust_sources(&path));
            }
        } else if name.ends_with(".rs") && !name.ends_with("tests.rs") {
            found.push(path);
        }
    }
    found
}

/// #32: the kernel cannot do I/O. Nothing it is built from reaches a
/// runtime, the OS or the network, and its source names no file, socket,
/// process, thread or clock.
#[test]
#[allow(clippy::disallowed_methods)]
fn the_kernel_does_no_io() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tree = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "kernel",
            "-e",
            "normal",
            "--prefix",
            "none",
            "-f",
            "{p}",
            "--offline",
        ])
        .current_dir(manifest)
        .output()
        .unwrap();
    assert!(
        tree.status.success(),
        "{}",
        String::from_utf8_lossy(&tree.stderr)
    );
    let tree = String::from_utf8(tree.stdout).unwrap();
    let crates: Vec<&str> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert!(
        crates.contains(&"protocol") && crates.contains(&"ext"),
        "{tree}"
    );
    for io in [
        "rt",
        "sys",
        "store",
        "engine",
        "ureq",
        "mio",
        "rustix",
        "tokio",
        "smol",
        "async-task",
        "async-io",
        "async-executor",
        "polling",
        "native-tls",
        "rustls",
        "socket2",
        "windows-sys",
    ] {
        assert!(!crates.contains(&io), "the kernel pulls in {io}:\n{tree}");
    }
    for file in std::fs::read_dir(manifest.join("src")).unwrap() {
        let path = file.unwrap().path();
        let source = std::fs::read_to_string(&path).unwrap();
        // `protocol` makes ids from the clock and the OS's randomness
        // (getrandom, over libc): the kernel is given its ids instead.
        let ids = ["SessionId::new", "ClientId::new", "now_v7", "Uuid::"];
        for io in [
            "std::fs",
            "std::net",
            "std::process",
            "std::thread",
            "std::io",
            "std::env",
        ]
        .into_iter()
        .chain(ids)
        {
            assert!(!source.contains(io), "{} names {io}", path.display());
        }
    }
}

/// #64: time comes in as events; the kernel's source reads no clock (and
/// its clippy.toml disallows it).
#[test]
fn the_kernel_never_reads_a_clock() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in std::fs::read_dir(manifest.join("src")).unwrap() {
        let path = file.unwrap().path();
        let source = std::fs::read_to_string(&path).unwrap();
        for clock in ["Instant", "SystemTime", "now()", "UNIX_EPOCH"] {
            assert!(!source.contains(clock), "{} names {clock}", path.display());
        }
    }
    let lint = std::fs::read_to_string(manifest.join("clippy.toml")).unwrap();
    assert!(
        lint.contains("std::time::Instant::now") && lint.contains("std::time::SystemTime::now")
    );
}

/// A child's watch fires once, however often it reports.
#[test]
fn a_watch_fires_once() {
    let load = Load {
        owner: Owner::None,
        ..Load::default()
    };
    let (mut s, _) = Session::resume(Reg::echo().arc(), Vec::new(), &load);
    s.step(user("task go"));
    s.step(Event::Spawn {
        intent: Json::null(),
        call: id(2),
        child: child(),
        input: "go".into(),
        wait: false,
    });
    for _ in 0..3 {
        s.step(Event::ChildSettled {
            child: child(),
            outcome: done(json!("ok")),
        });
    }
    s.step(tool_done(2, done(json!(null))));
    let fired = s
        .log
        .iter()
        .filter(|e| matches!(e.body, Body::Fired { .. }))
        .count();
    assert_eq!(fired, 1, "{:#?}", s.log);
}

/// The commit ending a run says how durable it must be: a barrier before
/// anything external, whichever setting; a note alone waits for the next
/// barrier under either; what the session learned mid-turn (a call
/// beginning to wait) waits only under `effects`; and a turn's end is a
/// barrier under either.
#[test]
fn a_run_commits_as_the_barriers_setting_says() {
    use kernel::{Barriers, Durability};
    let last = |fx: &StepPlan| fx.commit.expect("a step with appends").durability;
    let both = [
        (Barriers::Step, Durability::Barrier),
        (Barriers::Effects, Durability::Lazy),
    ];
    for (barriers, mid_turn) in both {
        let mut s = Session::with_barriers(Reg::modeled().arc(), barriers);
        // Input, then its model call sent: external, so barriers.
        assert_eq!(last(&s.step(user("hi"))), Durability::Barrier);
        s.step(prepared(2));
        let fx = s.step(model_done(2, &[("bash", json!({ "command": "ls" }))]));
        assert!(work(&fx).any(|e| matches!(e, Work::RunTool { .. })));
        assert_eq!(last(&fx), Durability::Barrier);
        let fx = s.step(Event::Note {
            call: id(4),
            note: Note::Progress("half".into()),
        });
        assert_eq!(last(&fx), Durability::Lazy, "{barriers:?}");
        let fx = s.step(Event::Waits {
            call: id(4),
            on: WaitOn::Client { tool: "ls".into() },
        });
        assert_eq!(last(&fx), mid_turn, "{barriers:?}");
        // The call's result, and the turn's end.
        s.step(Event::Answer {
            call: id(4),
            value: Json::null(),
            origin: None,
            ticket: None,
        });
        assert_eq!(
            last(&s.step(tool_done(4, done(json!(1))))),
            Durability::Barrier
        );
        s.step(prepared(9));
        let fx = s.step(model_done(9, &[]));
        assert_eq!(*s.core.turn(), Turn::Idle);
        assert_eq!(last(&fx), Durability::Barrier, "{barriers:?}");
    }
}

/// Nothing reaches outside the session (work started, a client answered)
/// before the entries ahead of it are a barrier, under either setting:
/// what every step of every test here holds too ([`common::barriered`]).
#[test]
fn no_external_effect_before_its_barrier() {
    for barriers in [kernel::Barriers::Step, kernel::Barriers::Effects] {
        let mut s = Session::with_barriers(Reg::modeled().arc(), barriers);
        let fx = s.step(user("hi"));
        drive(&mut s, into_work(fx));
        let fx = s.step(user("/jobs"));
        drive(&mut s, into_work(fx));
        let fx = s.step(interrupt());
        drive(&mut s, into_work(fx));
        assert_eq!(*s.core.turn(), Turn::Idle, "{:#?}", s.log);
    }
}

/// The calls set running, in the order they start.
fn running(fx: &StepPlan) -> impl Iterator<Item = CallId> + '_ {
    work(fx).filter_map(|e| match e {
        Work::RunTool { call, .. } => Some(*call),
        _ => None,
    })
}

fn asks_the_model(fx: &StepPlan) -> bool {
    work(fx).any(|e| matches!(e, Work::Prepare { .. }))
}

/// What call `call` asked of its tool, as its intent logged it.
fn asked_of(s: &Session, call: CallId) -> (String, String) {
    match &s.log[usize::try_from(call.get()).unwrap() - 1].body {
        Body::ToolCall { tool, input, .. } => (tool.clone(), input.to_string()),
        body => panic!("call {call} is {body:?}"),
    }
}

/// Shared calls of one reply all start in the step that logs it; the
/// agent goes on once every one has ended, in whatever order.
#[test]
fn calls_in_one_reply_run_concurrently() {
    let mut s = Session::new(Reg::modeled().arc());
    s.step(user("go"));
    let sleep = |n: u64| ("sleep", json!({ "seconds": n }));
    let fx = s.step(model_done(2, &[sleep(1), sleep(2), sleep(3)]));
    let started: Vec<u64> = running(&fx).map(CallId::get).collect();
    assert_eq!(started, [4, 5, 6], "{fx:#?}");
    assert!(matches!(s.core.turn(), Turn::Acting { open } if open.len() == 3));
    assert!(!asks_the_model(&s.step(tool_done(6, done(json!(3))))));
    assert!(!asks_the_model(&s.step(tool_done(4, done(json!(1))))));
    assert!(asks_the_model(&s.step(tool_done(5, done(json!(2))))));
}

/// Of a reply's calls, an exclusive one runs alone: it waits for the
/// shared calls before it, and those after it wait for it. Every call
/// starts in the order asked, whichever ends first.
#[test]
fn exclusive_tools_never_overlap() {
    let mut s = Session::new(Reg::modeled().arc());
    s.step(user("go"));
    let sleep = |n: u64| ("sleep", json!({ "seconds": n }));
    let bash = |c: &str| ("bash", json!({ "command": c }));
    let reply = [sleep(1), sleep(2), bash("a"), sleep(3), bash("b"), sleep(4)];
    let mut open: Vec<CallId> = running(&s.step(model_done(2, &reply))).collect();
    let mut started = open.clone();
    // The newest call ends first.
    while let Some(call) = open.pop() {
        let fx = s.step(tool_done(call.get(), done(json!(null))));
        open.extend(running(&fx));
        started.extend(running(&fx));
        let tools: Vec<String> = open.iter().map(|&c| asked_of(&s, c).0).collect();
        assert!(
            tools.len() <= 1 || tools.iter().all(|t| t == "sleep"),
            "{tools:?} run together"
        );
    }
    let asked: Vec<(String, String)> = (s.log.iter())
        .filter_map(|e| match &e.body {
            Body::ToolCall { tool, input, .. } => Some((tool.clone(), input.to_string())),
            _ => None,
        })
        .collect();
    let ran: Vec<(String, String)> = started.iter().map(|&c| asked_of(&s, c)).collect();
    assert_eq!(asked.len(), 6);
    assert_eq!(ran, asked);
}

#[test]
fn replay_safe_work_requires_ordering_even_when_its_step_appends_nothing() {
    let mut s = Session::new(Reg::modeled().arc());
    let first = s.step(user("hello"));
    assert!(first.admitted.iter().any(|guarded| guarded.admission
        == kernel::Admission {
            through: 2,
            requirement: kernel::Requirement::Ordered,
        }));
    let prepared = s.step(prepared(2));
    assert!(prepared.appends.is_empty(), "{prepared:?}");
    assert!(prepared.admitted.iter().any(|guarded| guarded.admission
        == kernel::Admission {
            through: 2,
            requirement: kernel::Requirement::Ordered,
        }));
}

#[test]
fn every_external_effect_has_an_explicit_admission() {
    for (name, mut session) in states() {
        for interrupted in [false, true] {
            let plan = if interrupted {
                session.core.step(interrupt())
            } else {
                session.core.resumed(&Load::default())
            };
            assert!(
                plan.immediate.iter().all(|work| !work.external()),
                "{name}: {plan:?}"
            );
            for guarded in &plan.admitted {
                assert_eq!(
                    guarded.admission.through,
                    session.core.ledger().seq(),
                    "{name}"
                );
                if matches!(
                    guarded.work,
                    Work::StartChild { .. } | Work::Deliver { .. } | Work::Reply { .. }
                ) {
                    assert_eq!(guarded.admission.requirement, kernel::Requirement::Durable);
                }
            }
        }
    }
}
