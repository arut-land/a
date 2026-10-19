use std::time::{Duration, Instant};

use protocol::Outcome;
use serde_json::{Value, json};

use super::*;
use crate::testing::Rig;
use crate::{DynTool, Extension, Hook, Policy, Registrar, builtins};

fn rig(name: &str) -> Rig {
    Rig::new(name, builtins(&Policy::default()).unwrap())
}

fn bash(rig: &Rig, input: &Value) -> Ran {
    serde_json::from_value(rig.done("bash", input)).unwrap()
}

fn run(rig: &Rig, command: &str) -> Ran {
    bash(rig, &json!({ "command": command }))
}

fn model(ran: &Ran) -> String {
    let llm::Output::Text(text) =
        DynTool::to_model(&Bash, &serde_json::to_value(ran).unwrap().into())
    else {
        panic!("bash answers in text")
    };
    text
}

fn wait_dead(pgid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while sys::group_alive(pgid) {
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
    assert!(sys::group_alive(pgid));
    assert_eq!(
        rig.spawned.lock().unwrap()[1],
        (pgid, true),
        "owned by the session"
    );
    assert!(model(&ran).contains(&format!(
        "[background processes still running: pgid {pgid}]"
    )));
}

#[test]
fn huge_output_is_cut_and_spilled_whole() {
    let rig = rig("huge");
    let ran = run(&rig, "seq 1 200000");
    let full = ran.full.clone().unwrap();
    let saved = std::fs::read_to_string(&full).unwrap();
    assert!(full.starts_with(rig.spill()), "{}", full.display());
    assert_eq!(full.file_name().unwrap(), "1.log");
    assert!(ran.truncated);
    assert_eq!(saved.lines().count(), 200_000);
    assert!(ran.output.starts_with("1\n2\n3\n"));
    assert!(ran.output.ends_with("199999\n200000\n"));
    assert!(ran.output.lines().count() <= super::super::output::LINES + 1);
    assert!(
        ran.output
            .contains(&format!("omitted; full output: {}]", full.display()))
    );
    assert!(model(&ran).ends_with("200000\n[exit code: 0]"));
}

#[test]
fn binary_and_terminal_output_are_cleaned() {
    let rig = rig("clean");
    let binary = run(&rig, "printf 'ELF\\0'; head -c 20000 /dev/zero");
    assert!(
        binary
            .output
            .starts_with("[binary output: 20004 bytes, saved to "),
        "{}",
        binary.output
    );
    let saved = std::fs::read(binary.full.unwrap()).unwrap();
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
        cwd: ws.join(dir),
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
        cwd: rig.workspace(),
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

#[test]
fn a_cancelled_call_stops_with_its_partial_output() {
    let rig = rig("cancel");
    let cancel = rig.cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        cancel.cancel();
    });
    let started = Instant::now();
    let ran = run(&rig, "echo partial; sleep 100");
    canceller.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
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
    rig.cancel.cancel();
    let started = Instant::now();
    let ran = run(&rig, "sleep 100");
    let job = bash(&rig, &json!({ "command": "sleep 100", "background": true }));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(ran.aborted && job.aborted, "{ran:?} {job:?}");
    assert_eq!(rig.spawned.lock().unwrap()[..], []);
    assert_eq!(model(&ran), "(no output)\n[aborted]");
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
    while std::fs::read_to_string(&job.log).unwrap_or_default() != "hi\n" {
        assert!(Instant::now() < deadline, "the log never filled");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(sys::group_alive(job.pgid));
    assert!(model(&ran).starts_with(&format!("[running in the background: pgid {}", job.pgid)));
}

/// Its end is an event, not something a poll finds: noticed within
/// moments, whether the shell itself or a process it left behind ends
/// last, and the leader is reaped (a zombie would keep the group alive).
#[test]
fn a_job_end_is_seen_at_once() {
    let rig = rig("job-ends");
    for command in ["sleep 0.3", "(sleep 0.3 &); exit 0"] {
        let mut seen = crate::job::ENDED.count();
        let ran = bash(&rig, &json!({ "command": command, "background": true }));
        let pgid = ran.background.unwrap().pgid;
        let started = Instant::now();
        rt::block_on(crate::job::ENDED.changed(&mut seen));
        let took = started.elapsed();
        assert!(!sys::group_alive(pgid), "{command}");
        assert!(took >= Duration::from_millis(250), "{command}: {took:?}");
        assert!(took < Duration::from_millis(1000), "{command}: {took:?}");
    }
}

/// Wraps every process in `env`, adding a variable.
struct Wrap;

impl Hook for Wrap {
    fn spawn(&self, spawn: &Spawn) -> Option<Spawn> {
        let mut argv = vec!["/usr/bin/env".to_owned(), "WRAPPED=1".to_owned()];
        argv.extend(spawn.argv.iter().cloned());
        Some(Spawn {
            argv,
            ..spawn.clone()
        })
    }
}

struct Wrapper;

impl Extension for Wrapper {
    const NAME: &'static str = "wrapper";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on(Wrap);
    }
}

#[test]
fn every_process_goes_through_the_spawn_hooks() {
    let registry = builtins(&Policy::default())
        .and_then(|r| r.with(&Wrapper))
        .unwrap();
    let rig = Rig::new("spawn-hook", registry);
    assert_eq!(run(&rig, "echo $WRAPPED").output, "1\n");
}

#[test]
fn invalid_input_fails_the_call() {
    let limit = &<Bash as Tool>::schema()["properties"]["timeout"]["maximum"];
    assert_eq!(*limit, MAX_TIMEOUT);
    let rig = rig("invalid");
    let outcome = rig.call("bash", &json!({ "cmd": "true" }));
    assert!(matches!(outcome, Outcome::Failed { error } if error.starts_with("invalid input")));
}
