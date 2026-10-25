use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use protocol::Outcome;
use serde_json::{Value, json};

use super::*;
use ext::host::Registry;
use ext::testing::Rig;
use ext::{Extension, Registrar, Rewrite};

/// Counts the heap the tests hold, and its peak.
pub struct Counting {
    now: AtomicUsize,
    peak: AtomicUsize,
}

#[global_allocator]
pub static ALLOC: Counting = Counting {
    now: AtomicUsize::new(0),
    peak: AtomicUsize::new(0),
};

impl Counting {
    /// Starts the peak over from what is held now, which it returns.
    pub fn reset(&self) -> usize {
        let now = self.now.load(Ordering::SeqCst);
        self.peak.store(now, Ordering::SeqCst);
        now
    }

    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

// SAFETY: every call goes to the system allocator as it came.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let now = self.now.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
        self.peak.fetch_max(now, Ordering::SeqCst);
        // SAFETY: as the caller asked.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.now.fetch_sub(layout.size(), Ordering::SeqCst);
        // SAFETY: as the caller asked.
        unsafe { System.dealloc(ptr, layout) }
    }
}

fn rig(name: &str) -> Rig {
    Rig::new(
        name,
        Registry::default().with(&crate::Shell::default()).unwrap(),
    )
}

fn bash(rig: &Rig, input: &Value) -> Ran {
    serde_json::from_value(rig.done("bash", input)).unwrap()
}

fn run(rig: &Rig, command: &str) -> Ran {
    bash(rig, &json!({ "command": command }))
}

/// A command's result, and what it holds outside the log.
fn kept(rig: &Rig, input: &Value) -> (Ran, Vec<Held>) {
    let called = rig.called("bash", input);
    let Outcome::Done {
        output: protocol::Payload::Inline(output),
    } = called.outcome
    else {
        panic!("{input}: {:?}", called.outcome)
    };
    (output.parse().unwrap(), called.held)
}

/// The blob a command's result holds, its only reference.
fn blob(held: &[Held]) -> &BlobRef {
    let [Held::Blob(blob)] = held else {
        panic!("{held:?}")
    };
    blob
}

/// The output stream of the rig's call `call`.
fn stream(rig: &Rig, call: u64) -> OutputStream {
    OutputStream {
        session: rig.session,
        call: protocol::CallId::new(call).unwrap(),
    }
}

fn model(ran: &Ran) -> String {
    let llm::Output::Text(text) = Bash::to_model(&Json::encode(ran).unwrap()) else {
        panic!("bash answers in text")
    };
    text
}

fn wait_dead(pgid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while sys::RecordedIdentity(pgid).alive() {
        assert!(Instant::now() < deadline, "group {pgid} survived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A group signal reaches the shell and its command at once: the shell
/// dies of it, or first reports its command did and exits 128 + `number`.
fn died_of(ran: &Ran, signal: &str, number: i32) -> bool {
    ran.signal.as_deref() == Some(signal) || ran.exit == Some(128 + number)
}

fn terminated(ran: &Ran) -> bool {
    died_of(ran, "SIGTERM", 15)
}

#[test]
fn output_interleaves_streams_and_trailers_end_with_the_exit_status() {
    let rig = rig("streams");
    let ran = run(&rig, "echo a; echo b >&2; echo c; exit 3");
    assert_eq!((ran.output.as_str(), ran.exit), ("a\nb\nc\n", Some(3)));
    assert_eq!(model(&ran), "a\nb\nc\n[exit code: 3]");
    let quiet = run(&rig, "true");
    assert_eq!(model(&quiet), "(no output)\n[exit code: 0]");
    let killed = run(&rig, "echo going; kill -9 $$");
    assert_eq!(killed.signal.as_deref(), Some("SIGKILL"));
    assert_eq!(killed.exit, None);
    assert_eq!(model(&killed), "going\n[killed by signal: SIGKILL]");
    let card = <Bash as Tool>::present_result(
        &Command {
            command: "echo going; kill -9 $$".into(),
            timeout: None,
            background: false,
        },
        &killed,
    )
    .unwrap();
    assert_eq!(
        (card.title.as_str(), card.note.as_str()),
        ("$ echo going; kill -9 $$", "killed by SIGKILL")
    );
}

#[test]
fn a_timeout_stops_the_whole_group_even_what_ignores_sigterm() {
    let rig = rig("timeout");
    let started = Instant::now();
    let ran = bash(
        &rig,
        &json!({ "command": "echo before; sleep 100 & trap '' TERM; sleep 100", "timeout": 1 }),
    );
    let took = started.elapsed();
    let (pgid, _) = rig.spawned.lock().unwrap()[0];
    wait_dead(pgid);
    assert_eq!(ran.timed_out, Some(1));
    assert!(died_of(&ran, "SIGKILL", 9), "{ran:?}");
    assert!(
        took >= Duration::from_secs(4) && took < Duration::from_secs(8),
        "{took:?}"
    );
    let text = model(&ran);
    assert!(text.starts_with("before\n"), "{text}");
    assert!(text.contains("\n[timed out after 1s]\n["), "{text}");
    let quick = bash(&rig, &json!({ "command": "sleep 100", "timeout": 1 }));
    assert!(terminated(&quick), "{quick:?}");
}

#[test]
fn a_background_child_holding_the_pipe_does_not_hold_the_call() {
    let rig = rig("linger");
    let started = Instant::now();
    let ran = run(&rig, "sleep 100 & echo started");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let (pgid, _) = rig.spawned.lock().unwrap()[0];
    assert_eq!((ran.output.as_str(), ran.exit), ("started\n", Some(0)));
    assert_eq!(ran.lingering, Some(pgid));
    assert!(sys::RecordedIdentity(pgid).alive());
    assert_eq!(
        rig.spawned.lock().unwrap()[1],
        (pgid, true),
        "owned by the session"
    );
    assert!(model(&ran).contains(&format!("[background processes still running: job {pgid}]")));
}

#[test]
fn a_lingering_groups_stream_is_sealed_as_the_group_ends_and_not_before() {
    let rig = rig("lingering-cut");
    let mut seen = rig.ends.count();
    let (ran, held) = kept(
        &rig,
        &json!({ "command": "(sleep 1; seq 1 50000) & seq 1 200000" }),
    );
    assert!(ran.truncated && ran.lingering.is_some(), "{ran:?}");
    assert_eq!(
        held,
        [Held::Stream(stream(&rig, 1))],
        "the stream, never a blob under a writer"
    );
    assert_eq!(stored(&rig), ["open"], "nothing sealed while it is written");
    rt::block_on(rig.ends.changed(&mut seen));
    let held = rig.held.lock().unwrap().clone();
    let blob = blob(&held);
    assert_eq!(blob.origin, BlobOrigin::ToolOutput(stream(&rig, 1).call));
    assert_eq!(read(&rig, blob), seq(1..=200_000) + &seq(1..=50_000));
    assert_open_is_empty(&rig);
}

#[test]
fn what_a_lingering_process_writes_after_a_short_command_is_kept() {
    let rig = rig("lingering-short");
    let mut seen = rig.ends.count();
    let (ran, held) = kept(
        &rig,
        &json!({ "command": "(sleep 1; echo later) & echo now" }),
    );
    assert!(!ran.truncated && ran.lingering.is_some(), "{ran:?}");
    assert_eq!(held, [Held::Stream(stream(&rig, 1))]);
    rt::block_on(rig.ends.changed(&mut seen));
    let held = rig.held.lock().unwrap().clone();
    assert_eq!(read(&rig, blob(&held)), "now\nlater\n");
    assert_open_is_empty(&rig);
}

#[test]
fn a_background_jobs_stream_is_sealed_into_exactly_what_it_wrote() {
    let rig = rig("job-sealed");
    for (command, wrote) in [
        ("seq 1 3000; printf end", seq(1..=3000) + "end"),
        ("true", String::new()),
    ] {
        let mut seen = rig.ends.count();
        bash(&rig, &json!({ "command": command, "background": true }));
        rt::block_on(rig.ends.changed(&mut seen));
        let held = std::mem::take(&mut *rig.held.lock().unwrap());
        if wrote.is_empty() {
            assert_eq!(held, [], "an empty stream is dropped, not kept");
        } else {
            let blob = blob(&held);
            assert_eq!(blob.media_type, "text/plain; charset=utf-8");
            assert_eq!(read(&rig, blob), wrote);
        }
        assert_open_is_empty(&rig);
    }
}

/// `seq`'s output over `numbers`.
fn seq(numbers: std::ops::RangeInclusive<u32>) -> String {
    numbers.fold(String::new(), |mut text, n| {
        let _ = writeln!(text, "{n}");
        text
    })
}

fn read(rig: &Rig, blob: &BlobRef) -> String {
    let mut text = String::new();
    rig.blobs
        .open(&blob.id)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// What the rig's blob store holds at its top.
fn stored(rig: &Rig) -> Vec<String> {
    let mut stored: Vec<_> = std::fs::read_dir(rig.dir.join("blobs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    stored.sort();
    stored
}

/// No stream is left growing once every group has ended.
fn assert_open_is_empty(rig: &Rig) {
    let open = rig.blobs.streams().join(rig.session.to_string());
    let left: Vec<_> = std::fs::read_dir(&open)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn huge_output_is_cut_and_kept_whole_as_a_blob() {
    let rig = rig("huge");
    let (ran, held) = kept(&rig, &json!({ "command": "seq 1 200000" }));
    let blob = blob(&held);
    let full = rig.blobs.places().blob(blob).unwrap();
    let saved = std::fs::read_to_string(&full).unwrap();
    assert_eq!(blob.size, saved.len() as u64);
    assert_eq!(
        blob.origin,
        BlobOrigin::ToolOutput(protocol::CallId::new(1).unwrap())
    );
    assert!(!rig.stream(1).exists(), "the stream was sealed");
    assert!(
        !Json::encode(&ran)
            .unwrap()
            .raw()
            .contains(&blob.id.to_string()),
        "the reference is held, not logged with the output"
    );
    assert!(ran.truncated);
    assert_eq!(saved.lines().count(), 200_000);
    assert!(ran.output.starts_with("1\n2\n3\n"));
    assert!(ran.output.ends_with("199999\n200000\n"));
    assert!(ran.output.lines().count() <= crate::output::LINES + 1);
    assert!(ran.output.contains(" omitted]\n"));
    assert!(model(&ran).ends_with("200000\n[exit code: 0]"));
}

#[test]
fn binary_and_terminal_output_are_cleaned() {
    let rig = rig("clean");
    let (binary, held) = kept(
        &rig,
        &json!({ "command": "printf 'ELF\\0'; head -c 20000 /dev/zero" }),
    );
    assert!(
        binary.output.starts_with("[binary output: 20004 bytes]"),
        "{}",
        binary.output
    );
    let blob = blob(&held);
    assert_eq!(blob.media_type, "application/octet-stream");
    let saved = std::fs::read(rig.blobs.places().blob(blob).unwrap()).unwrap();
    assert_eq!(saved.len(), 20004);
    let styled = run(
        &rig,
        r"printf '\033[31mred\033[0m\r\n10%%\r50%%\r100%%\n\a'",
    );
    assert_eq!(styled.output, "red\n100%\n");
    let invalid = run(&rig, r"printf 'a\377b\n'");
    assert_eq!(invalid.output, "a\u{FFFD}b\n");
}

#[test]
fn the_working_directory_persists_across_calls_and_restarts() {
    let rig = rig("cwd");
    let ws = rig.workspace();
    let moved = run(&rig, "mkdir -p sub/deeper && cd sub");
    assert_eq!(moved.cwd, ws.join("sub"));
    assert_eq!(
        run(&rig, "pwd").output,
        format!("{}\n", ws.join("sub").display())
    );
    run(&rig, "cd deeper");
    rig.restart();
    assert_eq!(
        run(&rig, "pwd").output,
        format!("{}\n", ws.join("sub/deeper").display())
    );
    // A directory removed since falls back to the workspace.
    run(&rig, "cd .. && rmdir deeper");
    run(
        &rig,
        "cd deeper 2>/dev/null; mkdir gone && cd gone && rmdir ../gone",
    );
    assert_eq!(
        run(&rig, "pwd").output,
        format!("{}\n", ws.join("sub").display())
    );
    // A failed `cd` and `exit` keep their status and directory.
    let failed = run(&rig, "cd /nonexistent");
    assert_eq!((failed.exit, failed.cwd.clone()), (Some(1), ws.join("sub")));
    let exited = run(&rig, "cd .. && exit 4");
    assert_eq!((exited.exit, exited.cwd.clone()), (Some(4), ws.clone()));
}

#[test]
fn a_moved_client_starts_commands_where_it_is() {
    let mut rig = rig("moved");
    let ws = rig.workspace();
    std::fs::create_dir_all(ws.join("a/b")).unwrap();
    let env = |dir: &str| Environment {
        cwd: ws.join(dir).try_into().unwrap(),
        vars: [("PATH".to_owned(), "/usr/bin:/bin".to_owned())].into(),
        ..Default::default()
    };
    rig.env = Some(env("a"));
    run(&rig, "cd b");
    assert_eq!(
        run(&rig, "pwd").output,
        format!("{}\n", ws.join("a/b").display())
    );
    rig.env = Some(env("."));
    assert_eq!(run(&rig, "pwd -P").output, format!("{}\n", ws.display()));
}

#[test]
fn commands_get_the_session_environment_quieted() {
    let mut rig = rig("env");
    rig.env = Some(Environment {
        cwd: rig.workspace().try_into().unwrap(),
        vars: [
            ("PATH", "/usr/bin:/bin"),
            ("ONLY_HERE", "1"),
            ("TERM", "xterm"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .into(),
        ..Default::default()
    });
    let ran = run(
        &rig,
        r#"echo "$ONLY_HERE [$HOME] $TERM $NO_COLOR $PAGER $GIT_PAGER $GIT_TERMINAL_PROMPT $A_MODEL"; echo "$A_SESSION_ID""#,
    );
    assert_eq!(
        ran.output,
        format!("1 [] dumb 1 cat cat 0 test:model\n{}\n", rig.session)
    );
}

#[test]
fn without_a_client_the_daemons_secrets_stay_out() {
    let rig = rig("secrets");
    let cx = rig.cx();
    let daemon = [
        "PATH",
        "OPENAI_API_KEY",
        "github_token",
        "DB_PASSWORD",
        "CLIENT_SECRET",
        "LANG",
    ]
    .map(|k| (k.to_owned(), "v".to_owned()));
    let env = environment(&cx, daemon.into_iter());
    assert!(env.contains_key("PATH") && env.contains_key("LANG"));
    for secret in [
        "OPENAI_API_KEY",
        "github_token",
        "DB_PASSWORD",
        "CLIENT_SECRET",
    ] {
        assert!(!env.contains_key(secret), "{secret}");
    }
}

#[test]
fn output_is_reported_as_it_arrives() {
    let rig = rig("progress");
    let ran = run(&rig, "echo one; sleep 0.3; echo two");
    let reported = rig.progress.lock().unwrap().clone();
    assert_eq!(ran.output, "one\ntwo\n");
    assert!(reported.starts_with("one\n"), "{reported:?}");
    assert!(ran.output.starts_with(&reported), "{reported:?}");
}

/// Output written as the command starts, with nothing after it, is
/// reported while the command runs: the log is watched before anything
/// can write to it, so no write comes before the watch.
#[test]
fn the_first_output_is_reported_while_the_command_runs() {
    let rig = rig("first-output");
    let started = Instant::now();
    let ran = std::thread::scope(|scope| {
        scope.spawn(|| {
            while rig.progress.lock().unwrap().is_empty() {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "never reported"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            rig.cancel.set();
        });
        run(&rig, "echo one; sleep 100")
    });
    assert!(started.elapsed() < Duration::from_secs(10));
    let reported = rig.progress.lock().unwrap().clone();
    assert!(reported.starts_with("one\n"), "{reported:?}");
    assert!(ran.aborted, "{ran:?}");
}

#[test]
fn a_cancelled_call_stops_with_its_partial_output() {
    let rig = rig("cancel");
    let started = Instant::now();
    let ran = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !rig.progress.lock().unwrap().starts_with("partial\n") {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "never reported"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            rig.cancel.set();
        });
        run(&rig, "echo partial; sleep 100")
    });
    assert!(ran.aborted && terminated(&ran), "{ran:?}");
    let text = model(&ran);
    assert!(text.starts_with("partial\n"), "{text}");
    assert!(text.contains("\n[aborted]\n["), "{text}");
}

/// A call cancelled before it starts, as an interrupted turn's is, starts
/// nothing.
#[test]
fn a_call_cancelled_before_it_starts_spawns_nothing() {
    let rig = rig("cancelled");
    rig.cancel.set();
    let started = Instant::now();
    let ran = run(&rig, "sleep 100");
    let job = bash(&rig, &json!({ "command": "sleep 100", "background": true }));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(ran.aborted && job.aborted, "{ran:?} {job:?}");
    assert_eq!(rig.spawned.lock().unwrap()[..], []);
    assert_eq!(model(&ran), "(no output)\n[aborted]");
}

#[test]
fn cancellation_during_launch_is_aborted_for_foreground_and_background_calls() {
    struct CancelSpawn(ext::Cancel);
    impl Extension for CancelSpawn {
        const NAME: &'static str = "cancel-spawn";
        const VERSION: u32 = 1;

        fn contribute(&self, r: &mut Registrar) {
            let cancel = self.0.clone();
            r.on::<ext::point::Spawn>(move |_, _| {
                cancel.set();
                Rewrite::Pass
            });
        }
    }

    for background in [false, true] {
        let cancel = ext::Cancel::default();
        let registry = Registry::default()
            .with(&crate::Shell::default())
            .and_then(|r| r.with(&CancelSpawn(cancel.clone())))
            .unwrap();
        let mut rig = Rig::new("cancel-during-launch", registry);
        rig.cancel = cancel.clone();
        // The spawn hook runs after bash's initial cancellation check.
        let ran = bash(
            &rig,
            &json!({ "command": "sleep 100", "background": background }),
        );
        assert!(cancel.is_set());
        assert!(ran.aborted, "{ran:?}");
        assert!(ran.background.is_none());
        assert!(rig.spawned.lock().unwrap().is_empty());
        assert_eq!(model(&ran), "(no output)\n[aborted]");
    }
}

#[test]
fn a_background_command_returns_at_once_with_its_log() {
    let rig = rig("background");
    let started = Instant::now();
    let ran = bash(
        &rig,
        &json!({ "command": "echo hi; sleep 100", "background": true }),
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    let job = ran.background.as_ref().unwrap();
    assert_eq!(rig.spawned.lock().unwrap()[..], [(job.pgid, true)]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let log = rig.stream(1);
    while std::fs::read_to_string(&log).unwrap_or_default() != "hi\n" {
        assert!(Instant::now() < deadline, "the log never filled");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(sys::RecordedIdentity(job.pgid).alive());
    assert!(model(&ran).starts_with(&format!("[running in the background: job {}", job.pgid)));
}

/// Its end is an event, not something a poll finds: noticed within
/// moments, whether the shell itself or a process it left behind ends
/// last, and the leader is reaped (a zombie would keep the group alive).
#[test]
fn a_job_end_is_seen_at_once() {
    let rig = rig("job-ends");
    for command in ["sleep 1", "(sleep 1 &); exit 0"] {
        let mut seen = rig.ends.count();
        let ran = bash(&rig, &json!({ "command": command, "background": true }));
        let pgid = ran.background.unwrap().pgid;
        let started = Instant::now();
        rt::block_on(rig.ends.changed(&mut seen));
        let took = started.elapsed();
        assert!(!sys::RecordedIdentity(pgid).alive(), "{command}");
        // Re-checking on a backoff would first look again at 1.55 s.
        assert!(took >= Duration::from_millis(900), "{command}: {took:?}");
        assert!(took < Duration::from_millis(1300), "{command}: {took:?}");
    }
}

/// Wraps every process in `env`, adding a variable.
struct Wrapper;

impl Extension for Wrapper {
    const NAME: &'static str = "wrapper";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<ext::point::Spawn>(|_, spawn| {
            let mut argv = vec!["/usr/bin/env".to_owned(), "WRAPPED=1".to_owned()];
            argv.extend(spawn.argv.iter().cloned());
            Rewrite::Replace(Spawn {
                argv,
                ..spawn.clone()
            })
        });
    }
}

#[test]
fn every_process_goes_through_the_spawn_hooks() {
    let registry = Registry::default()
        .with(&crate::Shell::default())
        .and_then(|r| r.with(&Wrapper))
        .unwrap();
    let rig = Rig::new("spawn-hook", registry);
    assert_eq!(run(&rig, "echo $WRAPPED").output, "1\n");
}

#[test]
fn invalid_input_fails_the_call() {
    let schema: Value = serde_json::from_str(&ext::schema::of::<Command>()).unwrap();
    let limit = &schema["properties"]["timeout"]["maximum"];
    assert_eq!(*limit, MAX_TIMEOUT);
    let rig = rig("invalid");
    let outcome = rig.call("bash", &json!({ "cmd": "true" }));
    assert!(matches!(outcome, Outcome::Failed { error } if error.starts_with("invalid input")));
}

#[test]
fn a_running_commands_output_is_on_disk() {
    let rig = rig("on-disk");
    let log = rig.stream(1);
    let ran = std::thread::scope(|s| {
        let call = s.spawn(|| run(&rig, "echo started; sleep 100"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(&log).unwrap_or_default() != "started\n" {
            assert!(
                Instant::now() < deadline,
                "the output never reached its file"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        rig.cancel.set();
        call.join().unwrap()
    });
    assert!(ran.aborted, "{ran:?}");
    // The shell may say its stopped child was terminated, as it stops too.
    let said = ran.output.strip_prefix("started\n");
    assert!(
        said.is_some_and(|rest| rest.is_empty() || rest.contains("Terminated")),
        "{ran:?}"
    );
    // Shown whole, so not kept.
    assert!(!log.exists());
}

#[test]
fn big_output_never_sits_in_memory() {
    let rig = rig("memory");
    run(&rig, "true");
    let before = ALLOC.reset();
    let (ran, held) = kept(
        &rig,
        &json!({ "command": "yes 'a line of output' | head -c 16000000" }),
    );
    let peak = ALLOC.peak() - before;
    assert!(ran.truncated && ran.exit == Some(0), "{ran:?}");
    let full = blob(&held);
    assert_eq!(full.size, 16_000_000);
    let path = rig.blobs.places().blob(full).unwrap();
    assert_eq!(std::fs::metadata(path).unwrap().len(), 16_000_000);
    assert!(peak < 4 << 20, "{peak} bytes at most while 16 MB ran");
}

#[test]
fn a_command_that_floods_its_output_is_stopped() {
    let rig = rig("flood");
    let (ran, held) = kept(&rig, &json!({ "command": "cat /dev/zero" }));
    assert_eq!(ran.flooded, Some(OUTPUT_CAP), "{ran:?}");
    assert_eq!(blob(&held).size, OUTPUT_CAP, "kept up to its cap");
    assert!(model(&ran).contains("[stopped: its output passed 64 MiB]"));
}

/// The supervisor's transitions.
#[test]
fn the_supervisor_moves_only_as_its_states_allow() {
    use std::os::unix::process::ExitStatusExt;

    let t = Instant::now();
    let later = t + Duration::from_secs(1);
    let status = ExitStatus::from_raw(0);
    let running = Run::Running {
        deadline: Some(later),
    };
    let term = |why| Run::Stopping {
        why,
        kill: KillPhase::Term { at: t },
    };
    let exited = |why| Run::Exited { status, why, at: t };
    let seen = Seen::default();
    let exit = Seen {
        exited: Some(status),
        ..seen
    };
    let cancel = Seen {
        cancelled: true,
        ..seen
    };
    let flood = Seen {
        flooded: true,
        ..seen
    };
    let killed = Run::Stopping {
        why: Stop::Aborted,
        kill: KillPhase::Killed,
    };
    let term_signal = Some(ProcessSignal::Terminate);
    let timed_out = Run::Stopping {
        why: Stop::TimedOut,
        kill: KillPhase::Term { at: later },
    };
    let table = [
        (running, t, seen, running, None),
        (running, later, seen, timed_out, term_signal),
        (running, t, cancel, term(Stop::Aborted), term_signal),
        (running, t, flood, term(Stop::Flooded), term_signal),
        (running, t, exit, exited(None), None),
        // Stopping: a cancel changes nothing; the grace runs out.
        (term(Stop::TimedOut), t, cancel, term(Stop::TimedOut), None),
        (
            term(Stop::Aborted),
            t + GRACE,
            seen,
            killed,
            Some(ProcessSignal::Kill),
        ),
        (killed, later, seen, killed, None),
        (
            term(Stop::Aborted),
            t,
            exit,
            exited(Some(Stop::Aborted)),
            None,
        ),
        // Exited absorbs everything.
        (exited(None), later, cancel, exited(None), None),
        (exited(None), later, flood, exited(None), None),
    ];
    for (from, now, seen, to, signal) in table {
        let (state, sent) = from.step(now, seen);
        let kind = |signal| std::mem::discriminant(&signal);
        assert_eq!(
            (state, sent.map(kind)),
            (to, signal.map(kind)),
            "{from:?} seeing {seen:?}"
        );
    }
    assert_eq!(running.deadline(t), Some(later));
    assert_eq!(term(Stop::Aborted).deadline(t), Some(t + GRACE));
    assert_eq!(killed.deadline(t), None);
    assert_eq!(exited(None).deadline(t), Some(t + IDLE));
}

#[test]
fn continuous_output_yields_to_other_sessions() {
    use std::io::Write;
    let rig = rig("output-quantum");
    let cx = rig.cx();
    rt::block_on(async {
        let Stream { mut file, open } = cx.blobs.stream(cx.call, u64::MAX).unwrap();
        let log = open.path().to_owned();
        file.write_all(&vec![b'x'; usize::try_from(READ * 3).unwrap()])
            .unwrap();
        let written = rt::Async::for_reading(
            &runtime(&cx).unwrap().reactor(),
            sys::write_watch(&log).unwrap(),
        )
        .unwrap();
        let process = cx
            .service::<dyn Processes>()
            .unwrap()
            .spawn(
                &cx,
                LaunchSpec {
                    process: spawn_with::<crate::dialect::Bash>(
                        sys::shell::ExecutionFacts::current(),
                        "exit 0",
                        &cx.dir(),
                        environment(&cx, std::env::vars()),
                        None,
                    )
                    .unwrap(),
                    output: ProcessOutput::File(file),
                },
            )
            .await
            .unwrap();
        process.wait().await.unwrap();
        let advanced = ext::Flag::default();
        let observed = &advanced;
        let cx = &cx;
        let log = &log;
        let mut work = ext::stream(|progress| async move {
            let detached = Detached {
                dir: cx.dir(),
                started: 0,
            };
            let supervised = Supervised::Launched {
                limit: Duration::from_secs(5),
                detached: &detached,
            };
            supervise::<crate::dialect::Bash>(
                process,
                Followed { log, written },
                supervised,
                cx,
                &progress,
            )
            .await
            .unwrap();
            assert!(
                observed.is_set(),
                "output consumed every quantum before another session ran"
            );
            Ok::<_, String>(())
        });
        rt::or(
            async { while !matches!(work.next().await, shape::Next::Done(Ok(()))) {} },
            async {
                advanced.set();
                std::future::pending::<()>().await;
            },
        )
        .await;
    });
}

#[test]
fn background_output_cannot_exceed_the_output_cap() {
    let rig = rig("bounded-background");
    let (ran, held) = kept(&rig, &json!({ "command": "yes x", "background": true }));
    let job = ran.background.as_ref().unwrap();
    assert_eq!(held, [Held::Stream(stream(&rig, 1))]);
    let log = rig.stream(1);
    wait_dead(job.pgid);
    assert_eq!(std::fs::metadata(log).unwrap().len(), OUTPUT_CAP);
}

#[test]
fn unterminated_terminal_metadata_cannot_grow_retained_output() {
    let (mut output, mut live) = (Output::default(), Live::default());
    let baseline = ALLOC.reset();
    output.push(b"\x1b]0;");
    live.push(b"\x1b]0;");
    let chunk = [b'x'; 4096];
    for _ in 0..2048 {
        output.push(&chunk);
        live.push(&chunk);
    }
    assert!(ALLOC.peak().saturating_sub(baseline) < 256 << 10);
    assert_eq!(output.finish().render().text, "");
    assert_eq!(live.take(), None);
}
