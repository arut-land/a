//! M1 acceptance: the engine's promises, checked against the built binary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use crate::common::{Env, Repl, assert_contiguous, group_alive, is_tool_call, result_of, wait_for};
use protocol::{
    Audience, Body, CallId, ClientMsg, Json, Mode, Outcome, Permission, Target, Verdict,
};
use serde_json::json;

const ANY: Target = Target::Latest {
    any_workspace: true,
};

#[test]
fn concurrent_cold_starts_make_exactly_one_daemon() {
    let env = Env::new("one-daemon");
    let clients: Vec<_> = (0..8).map(|_| env.cmd(&["ls"]).spawn().unwrap()).collect();
    for mut client in clients {
        assert!(client.wait().unwrap().success());
    }
    assert_eq!(env.daemons(), [env.pid().unwrap()]);
}

/// The cold client binds the socket itself and connects into the new
/// daemon's backlog, so it never sleeps waiting for the daemon to come up.
/// Only the client is traced: strace would otherwise wait out the daemon.
#[cfg(target_os = "linux")]
#[test]
fn a_cold_client_never_polls_for_the_daemon() {
    let env = Env::new("no-polling");
    let trace = env.root.join("trace");
    let wrapper = [
        "strace",
        "-e",
        "trace=connect,nanosleep,clock_nanosleep",
        "-o",
        trace.to_str().unwrap(),
    ];
    let status = env.cmd_under(&wrapper, &["ls"]).status();
    assert!(status.expect("this test needs strace").success());
    let trace = fs::read_to_string(trace).unwrap();
    assert!(trace.lines().any(|l| l.starts_with("connect(")), "{trace}");
    assert!(!trace.contains("sleep("), "{trace}");
}

#[test]
fn resume_after_the_terminal_closes_mid_turn_replays_every_entry() {
    let env = Env::new("resume");
    let mut term = env.repl(&[]);
    term.lines_until("[session");
    term.type_line("hello");
    term.type_line("sleep 1");
    term.lines_until("tool sleep");
    let hangup = Command::new("kill")
        .args(["-HUP", &term.child.id().to_string()])
        .status();
    assert!(hangup.unwrap().success());
    term.child.wait().unwrap();

    let mut conn = env.connect();
    let (id, mut entries) = conn.attach(ANY, &env.ws);
    let call = entries.iter().find(|e| is_tool_call(e)).unwrap().seq;
    if !entries.iter().any(|e| result_of(e, call).is_some()) {
        entries.extend(conn.until(|e| result_of(e, call).is_some()));
    }
    assert_contiguous(&entries);
    assert_eq!(entries, env.log(id));
    // Input typed while a turn runs is logged as waiting first.
    let mut bodies: Vec<_> = entries
        .into_iter()
        .map(|e| e.body)
        .filter(|b| !matches!(b, Body::Input { taken: false, .. } | Body::Take { .. }))
        .collect();
    // Which client sent each, under which number, is its own.
    for body in &mut bodies {
        if let Body::Input { origin, .. } = body {
            assert!(origin.take().is_some(), "{body:?}");
        }
    }
    let taken = |text: &str| Body::Input {
        text: text.into(),
        mode: Mode::Queue,
        taken: true,
        origin: None,
    };
    let opened = bodies.remove(0);
    assert!(matches!(opened, Body::Extensions { .. }), "{opened:?}");
    let attached = bodies.remove(0);
    assert!(
        matches!(&attached, Body::Environment(shell) if shell.cwd == env.ws),
        "{attached:?}"
    );
    assert_eq!(
        bodies,
        [
            taken("hello"),
            Body::Assistant {
                text: "hello".into()
            },
            taken("sleep 1"),
            Body::ToolCall {
                tool: "sleep".into(),
                input: Json::encode(&json!({ "seconds": 1 })).unwrap(),
                id: None,
                card: None,
                by: Audience::Model,
            },
            Body::ToolResult {
                call: CallId(call),
                outcome: Outcome::Done {
                    output: Json::encode(&json!({ "slept": 1 })).unwrap()
                },
                card: None,
            },
        ]
    );

    let mut resumed = env.repl(&["resume"]);
    let mut shown = resumed.lines_until("└");
    shown.remove(0);
    assert_eq!(
        shown,
        [
            &format!("  · env {}", env.ws.display()),
            "you  hello",
            "a    hello",
            "you  sleep 1",
            r#"tool sleep {"seconds":1}"#,
            r#"  │ {"slept":1}"#,
            "  └ done",
        ]
    );
    assert_eq!(resumed.line(), "[recap · you: sleep 1 · ran sleep]");
    resumed.finish();
}

#[test]
fn two_terminals_on_one_session_see_identical_state() {
    let env = Env::new("two-terminals");
    env.start();
    let mut first = env.connect();
    let (id, opened) = first.attach(Target::New, &env.ws);
    let mut second = env.connect();
    assert_eq!(second.attach(Target::Id(id), &env.ws).1, opened);

    for text in ["one", "$ echo two", "sleep 0"] {
        first.input(text);
    }
    let done = |e: &protocol::Entry| match &e.body {
        Body::ToolResult {
            outcome: Outcome::Done { output },
            ..
        } => output.parse::<serde_json::Value>().unwrap()["slept"] == 0,
        _ => false,
    };
    let seen = [first.until(done), second.until(done)].map(|live| [opened.clone(), live].concat());
    let mut third = env.connect();
    let (_, replayed) = third.attach(Target::Id(id), &env.ws);

    assert_contiguous(&seen[0]);
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], replayed);
    assert_eq!(seen[0], env.log(id));
    let hashes = [&first, &second, &third].map(|c| c.view.state_hash());
    assert_eq!(hashes, [hashes[0]; 3], "{:#?}", [first.view, third.view]);
    assert_ne!(first.view.items, []);
}

#[test]
fn a_safe_tool_reruns_after_the_daemon_is_killed() {
    let env = Env::new("kill-safe");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    conn.input("sleep 2");
    let call = conn.until(is_tool_call).pop().unwrap().seq;
    env.kill_daemon();
    let torn = env.log(id);

    env.start();
    let mut conn = env.connect();
    let (_, mut entries) = conn.attach(Target::Id(id), &env.ws);
    if !entries.iter().any(|e| result_of(e, call).is_some()) {
        entries.extend(conn.until(|e| result_of(e, call).is_some()));
    }

    // Its extensions, the input and the call.
    assert_eq!(torn.len(), 3);
    assert_contiguous(&entries);
    assert_eq!(entries, env.log(id));
    env.run(&["daemon", "stop"]);
    let diagnostic = env.run(&["daemon", "log"]);
    let records: Vec<serde_json::Value> = diagnostic
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        records
            .iter()
            .any(|r| r["fact"]["recovery"]["session"] == id.0.to_string()
                && r["fact"]["recovery"]["rule"] == "resume_ledger"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("sleep 2"));
    let results: Vec<_> = entries.iter().filter_map(|e| result_of(e, call)).collect();
    assert_eq!(
        results,
        [&Outcome::Done {
            output: Json::encode(&json!({ "slept": 2 })).unwrap()
        }]
    );
}

#[test]
fn an_unsafe_tool_is_interrupted_with_its_output_and_its_processes_killed() {
    let env = Env::new("kill-unsafe");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    conn.input("$ echo ran >> ran.txt; echo started; sleep 30");
    let entries = conn.until(|e| matches!(e.body, Body::ToolProcess { .. }));
    let Body::ToolProcess { call, pgid, .. } = entries.last().unwrap().body else {
        unreachable!()
    };
    let ran = env.ws.join("ran.txt");
    wait_for("the command to start", || ran.exists());
    // Its output so far is committed as it runs.
    conn.until(|e| matches!(&e.body, Body::Progress { call: c, .. } if *c == call));
    env.kill_daemon();
    let orphaned = group_alive(pgid);

    env.start();
    let mut conn = env.connect();
    let (_, mut entries) = conn.attach(Target::Id(id), &env.ws);
    if !entries.iter().any(|e| result_of(e, call.0).is_some()) {
        entries.extend(conn.until(|e| result_of(e, call.0).is_some()));
    }
    wait_for("the leftover process group to die", || !group_alive(pgid));

    assert!(orphaned, "the tool's processes died with the daemon");
    assert_contiguous(&entries);
    assert_eq!(entries, env.log(id));
    let results: Vec<_> = entries
        .iter()
        .filter_map(|e| result_of(e, call.0))
        .collect();
    assert_eq!(
        results,
        [&Outcome::Interrupted {
            output: Some("started\n".into())
        }]
    );
    assert_eq!(fs::read_to_string(ran).unwrap(), "ran\n");
}

#[test]
fn a_command_run_by_the_agent_uses_the_attaching_shells_path_and_virtualenv() {
    let env = Env::new("shell-env");
    let venv = env.root.join("venv");
    fs::create_dir_all(venv.join("bin")).unwrap();
    let python = venv.join("bin/python");
    fs::write(&python, "#!/bin/sh\necho \"venv python in $VIRTUAL_ENV\"\n").unwrap();
    fs::set_permissions(&python, fs::Permissions::from_mode(0o755)).unwrap();
    let start = || {
        let started = env.cmd(&["ls"]).env_remove("VIRTUAL_ENV").status();
        assert!(started.unwrap().success());
    };
    start();

    let mut daemons = env.connect();
    daemons.attach(Target::New, &env.ws);
    let bare = daemons.run_command(r#"echo "[$VIRTUAL_ENV]""#);
    assert_eq!(bare["output"], "[]\n", "the daemon runs outside the venv");

    let mut shell = env.repl_from(&[], |cmd| {
        cmd.env(
            "PATH",
            format!("{}:/usr/bin:/bin", venv.join("bin").display()),
        )
        .env("VIRTUAL_ENV", &venv)
        .env("DEPLOY_TOKEN", "s3cret");
    });
    let session = shell.lines_until("[session").pop().unwrap();
    let check = r#"python; [ -n "$DEPLOY_TOKEN" ] && echo has-token || echo no-token"#;
    shell.type_line(&format!("$ {check}"));
    let shown = shell.lines_until("└");
    let in_venv = format!("  │ venv python in {}", venv.display());
    assert!(shown.contains(&in_venv), "{shown:?}");
    assert!(shown.contains(&"  │ has-token".to_owned()), "{shown:?}");
    shell.finish();

    // The environment outlives the daemon, without the secret's value.
    env.run(&["daemon", "stop"]);
    start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(ANY, &env.ws);
    assert!(session.contains(&id.short().to_string()), "{session}");
    let after = conn.run_command(check);
    assert_eq!(
        after["output"],
        format!("venv python in {}\nno-token\n", venv.display())
    );
    let log = fs::read_to_string(env.home.sessions_dir().join(format!("{id}.jsonl"))).unwrap();
    assert!(
        log.contains("DEPLOY_TOKEN") && !log.contains("s3cret"),
        "{log}"
    );

    // A refresh (`/env refresh`) from a shell that left the venv.
    let mut outside = protocol::Environment {
        cwd: env.ws.clone(),
        ..Default::default()
    };
    outside.vars.insert("PATH".into(), "/usr/bin:/bin".into());
    conn.send(&ClientMsg::Environment(outside));
    let refreshed = conn.run_command(r#"echo "[$VIRTUAL_ENV]""#);
    assert_eq!(refreshed["output"], "[]\n");
}

#[test]
fn the_shell_keeps_its_directory_across_a_daemon_restart() {
    let env = Env::new("shell-cwd");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    let moved = conn.run_command("mkdir -p sub && cd sub");
    assert_eq!(moved["cwd"], env.ws.join("sub").display().to_string());
    drop(conn);
    env.run(&["daemon", "stop"]);

    env.start();
    let mut conn = env.connect();
    conn.attach(Target::Id(id), &env.ws);
    let here = conn.run_command("pwd");
    assert_eq!(
        here["output"],
        format!("{}\n", env.ws.join("sub").display())
    );
    let log = env.log(id);
    assert!(
        log.iter().any(
            |e| matches!(&e.body, Body::Ext { ext, state, .. } if ext == "shell" && state == "cwd")
        ),
        "{log:#?}"
    );
}

#[test]
fn entries_after_a_torn_tail_survive_a_restart() {
    let env = Env::new("torn-tail");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    conn.input("one");
    conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    drop(conn);
    env.run(&["daemon", "stop"]);
    let log = env.home.sessions_dir().join(format!("{id}.jsonl"));
    let mut text = fs::read_to_string(&log).unwrap();
    text.push_str(r#"{"seq":4,"ep"#);
    fs::write(&log, text).unwrap();

    env.start();
    let mut conn = env.connect();
    conn.attach(Target::Id(id), &env.ws);
    conn.input("two");
    conn.until(|e| matches!(e.body, Body::Assistant { .. }));
    drop(conn);
    env.run(&["daemon", "stop"]);

    env.start();
    let (_, entries) = env.connect().attach(Target::Id(id), &env.ws);
    let queued = |e: &&protocol::Entry| {
        matches!(e.body, Body::Input { taken: false, .. } | Body::Take { .. })
    };
    assert_eq!(entries.iter().filter(|e| !queued(e)).count(), 5);
    assert_contiguous(&entries);
    assert_eq!(entries, env.log(id));
}

#[test]
fn a_parents_watch_fires_exactly_once_after_the_daemon_is_killed_mid_child() {
    let env = Env::new("watch");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    conn.input("spawn sleep 2");
    let linked = conn.until(|e| matches!(e.body, Body::Watch { .. }));
    let Some(Body::Link { child, .. }) = linked
        .iter()
        .map(|e| &e.body)
        .find(|b| matches!(b, Body::Link { .. }))
        .cloned()
    else {
        panic!("no link in {linked:?}")
    };
    // The child is mid-sleep when the daemon dies.
    let mut child_conn = env.connect();
    let (_, started) = child_conn.attach(Target::Id(child), &env.ws);
    if !started.iter().any(is_tool_call) {
        child_conn.until(is_tool_call);
    }
    env.kill_daemon();
    let torn = (env.log(id), env.log(child));

    env.start();
    let mut conn = env.connect();
    let (_, mut entries) = conn.attach(Target::Id(id), &env.ws);
    let is_fired = |e: &protocol::Entry| matches!(e.body, Body::Fired { .. });
    if !entries.iter().any(is_fired) {
        entries.extend(conn.until(is_fired));
    }
    drop(conn);
    env.run(&["daemon", "stop"]);
    // Another restart re-arms nothing: the watch has fired.
    env.start();
    let (_, replayed) = env.connect().attach(Target::Id(id), &env.ws);

    assert!(!torn.0.iter().any(is_fired), "fired before the kill");
    let call = torn.1.iter().find(|e| is_tool_call(e)).unwrap().seq;
    assert!(!torn.1.iter().any(|e| result_of(e, call).is_some()));
    assert_contiguous(&replayed);
    assert_eq!(replayed, env.log(id));
    let fired: Vec<_> = replayed
        .iter()
        .filter_map(|e| match &e.body {
            Body::Fired { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect();
    let slept = Outcome::Done {
        output: Json::encode(&json!({ "slept": 2 })).unwrap(),
    };
    assert_eq!(fired, [&slept]);
    let child_log = env.log(child);
    assert_contiguous(&child_log);
    let results: Vec<_> = child_log
        .iter()
        .filter_map(|e| result_of(e, call))
        .collect();
    assert_eq!(results, [&slept]);
}

#[test]
fn sessions_outlive_their_daemon() {
    let env = Env::new("outlive");
    let id = env.run(&["new"]).trim().to_owned();
    assert_eq!(env.run(&["daemon", "stop"]), "stopped\n");
    let listed = env.run(&["ls"]);
    assert!(listed.starts_with(&id), "{listed}");
    assert!(listed.contains("  idle       hibernated  "), "{listed}");
}

#[test]
fn a_pending_approval_survives_detach_and_is_re_delivered_on_attach() {
    let env = Env::new("approval");
    env.ext("policy", "ask = [\"bash\"]");
    env.start();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    conn.input("$ echo ran > ran.txt");
    let asks = |body: &Body| {
        matches!(
            body,
            Body::Decision {
                d: Verdict::Permission {
                    permission: Permission::Ask
                },
                ..
            }
        )
    };
    let asked = conn.until(|e| asks(&e.body)).pop().unwrap();
    let Body::Decision {
        call: Some(call), ..
    } = asked.body
    else {
        unreachable!()
    };
    assert_eq!(conn.view.ledger.inbox().next(), Some(call));
    let listed = env.run(&["ls"]);
    assert!(listed.contains("  approval   loaded  "), "{listed}");
    drop(conn);

    let mut conn = env.connect();
    let (_, replayed) = conn.attach(Target::Id(id), &env.ws);
    assert!(replayed.iter().any(|e| e.seq == asked.seq && asks(&e.body)));
    let recap = protocol::view::Recap {
        prompt: Some("$ echo ran > ran.txt".into()),
        tools: vec!["bash".into()],
        asks: 1,
        ..protocol::view::Recap::default()
    };
    assert_eq!(conn.view.recap, Some(recap));
    assert_eq!(conn.view.ledger.inbox().next(), Some(call));
    drop(conn);

    env.kill_daemon();
    env.start();
    let ran = env.ws.join("ran.txt");
    assert!(!ran.exists(), "the call ran unapproved");
    let mut term = env.repl(&["resume"]);
    let shown = term.lines_until("?");
    assert_eq!(
        &shown[shown.len() - 2..],
        [
            "[recap · you: $ echo ran > ran.txt · ran bash · 1 waiting for approval]",
            "? $ echo ran > ran.txt  waiting for approval  [y/n]",
        ]
    );
    term.type_line("y");
    assert_eq!(term.lines_until("└"), ["  · approved", "  └ exit 0"]);
    term.finish();

    assert_eq!(fs::read_to_string(ran).unwrap(), "ran\n");
    let log = env.log(id);
    assert_contiguous(&log);
    let answers: Vec<_> = log
        .iter()
        .filter(|e| matches!(e.body, Body::Answer { .. }))
        .collect();
    assert!(
        matches!(&answers[..], [e] if matches!(&e.body, Body::Answer { call: c, value, .. }
            if *c == call && value.parse::<bool>().is_ok_and(|yes| yes))),
        "{log:#?}"
    );
}

/// Two installs of `a` in one home, such as the app's bundled build and an
/// older CLI: the newer build takes over once, by a drain the attached older
/// client rides out, and from then on both are served by its daemon. The
/// older one never drains it back.
#[test]
fn an_older_and_a_newer_build_share_one_daemon_and_never_fight() {
    let env = Env::new("two-builds");
    let old = env.older_build();
    let run_old = |args: &[&str]| {
        let out = env.cmd_as(&old, args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let runtime = env.home.runtime();

    // Only the older build has run: its daemon serves.
    let id = run_old(&["new"]).trim().to_owned();
    let first = env.pid().unwrap();
    let old_id = runtime.current().unwrap();
    let mut term = Repl::spawn(env.cmd_as(&old, &["attach", &id]));
    term.lines_until("[session");

    // The newer build stages itself as current and drains the old daemon;
    // the attached older client reconnects to the new one.
    assert!(env.run(&["ls"]).starts_with(&id));
    let new_id = runtime.current().unwrap();
    assert_ne!(new_id, old_id);
    assert_eq!(
        fs::read(runtime.current_exe()).unwrap(),
        fs::read(&env.exe).unwrap(),
        "current is not the newer build"
    );
    let second = env.pid().unwrap();
    assert_ne!(second, first);
    let log = fs::read_to_string(env.home.daemon_log()).unwrap();
    assert!(log.contains("\"reason\":\"upgrade\""), "{log}");
    term.type_line("still here");
    assert_eq!(
        term.lines_until("a    still here").last().unwrap(),
        "a    still here"
    );

    // From now on either build is welcomed by that daemon.
    for _ in 0..3 {
        assert!(run_old(&["ls"]).starts_with(&id));
        assert!(env.run(&["ls"]).starts_with(&id));
    }
    assert_eq!(env.daemons(), [second]);
    assert_eq!(runtime.current().unwrap(), new_id);
    term.finish();
}
