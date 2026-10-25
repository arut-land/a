//! Upgrades that keep work running: a drain left with only calls gone on
//! in the background and the jobs sessions own hands them, live, to the
//! newer build (crates/engine/src/handover.rs), which goes on with them as
//! if nothing happened; one whose newer build cannot take them waits for
//! them to end.

use std::fs;
use std::path::Path;

use protocol::{Body, Entry, EnvelopeId, Held, SessionId, Target};

use crate::common::{Conn, Env, group_alive, wait_for, wait_within};

/// The jobs and the call of one session, started on the older build:
/// `sleep 300` for a job only `a daemon stop` ends, a job that writes
/// `late` and ends, and a call that goes on in the background past its
/// one-second timeout and exits 7.
fn running_work(env: &Env, old: &Path) -> (Conn, SessionId) {
    let out = env.cmd_as(old, &["ls"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    for command in [
        "sleep 300 & echo server",
        "(sleep 14; echo late) & echo now",
    ] {
        conn.input(&format!("$ {command}"));
        conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    }
    conn.input("$ echo start; sleep 8; echo end; exit 7");
    conn.until(|e| matches!(e.body, Body::Detached { .. }));
    (conn, id)
}

/// The groups the session's jobs run in, as its log records them.
fn jobs(env: &Env, id: SessionId) -> Vec<u32> {
    (env.log(id).iter())
        .filter_map(|e| match e.body {
            Body::ToolProcess {
                pgid,
                background: true,
                ..
            } => Some(pgid),
            _ => None,
        })
        .collect()
}

/// Whether a sealed blob under the home holds exactly `text`.
fn blob_holds(env: &Env, text: &str) -> bool {
    let Ok(fans) = fs::read_dir(env.home.blobs_dir()) else {
        return false;
    };
    fans.flatten()
        .filter(|fan| fan.file_name() != "open")
        .filter_map(|fan| fs::read_dir(fan.path()).ok())
        .flatten()
        .flatten()
        .any(|blob| fs::read(blob.path()).is_ok_and(|bytes| bytes == text.as_bytes()))
}

fn detached_outcome(entries: &[Entry]) -> Option<&str> {
    entries.iter().find_map(|e| match &e.body {
        Body::Enqueue { envelope } if envelope.id.purpose == EnvelopeId::DETACHED => {
            Some(envelope.payload.as_str())
        }
        _ => None,
    })
}

fn job_ended_holding_a_blob(entries: &[Entry]) -> bool {
    entries.iter().any(|e| {
        matches!(&e.body, Body::Enqueue { envelope }
            if envelope.id.purpose == EnvelopeId::JOB
                && matches!(envelope.held.as_slice(), [Held::Blob(_)]))
    })
}

/// A build newer than the one under test: the same program with two
/// bytes more, built an hour later.
fn newer_build(env: &Env) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let exe = env.root.join("bin").join("a-new");
    let mut bytes = fs::read(&env.exe).unwrap();
    bytes.extend([0, 0]);
    fs::write(&exe, bytes).unwrap();
    let file = fs::File::options().write(true).open(&exe).unwrap();
    let built = fs::metadata(&env.exe).unwrap().modified().unwrap();
    file.set_modified(built + std::time::Duration::from_secs(3600))
        .unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o755))
        .unwrap();
    exe
}

/// The older build's daemon is running a call in the background and two
/// jobs when the newer build's first client drains it: it hands them over
/// and becomes the newer build in its own process, and that build hands
/// them on again to a newer one still. The call's outcome (exit 7, its
/// whole output) and a job's end (its output sealed) reach the session as
/// they would have, no crash is counted, and the adopted groups are the
/// last daemon's to stop.
#[test]
fn an_upgrade_hands_running_work_to_the_newer_build_live() {
    let env = Env::new("handover");
    env.ext("shell", "timeout = 1");
    let old = env.older_build();
    let (conn, id) = running_work(&env, &old);
    let pid = env.pid().unwrap();
    let jobs = jobs(&env, id);
    assert_eq!(jobs.len(), 2, "{jobs:?}");

    // Each newer build's first client drains the daemon, which hands its
    // work over; the client is served by that build in the same process.
    let runtime = env.home.runtime();
    for build in [env.exe.clone(), newer_build(&env)] {
        let out = env.cmd_as(&build, &["ls"]).output().unwrap();
        let listed = String::from_utf8_lossy(&out.stdout);
        assert!(listed.starts_with(&id.short().to_string()), "{listed}");
        assert_eq!(env.pid(), Some(pid), "the daemon left its process");
        assert_eq!(
            client::diagnose(&env.home).record.map(|r| r.build),
            runtime.current(),
            "the daemon is not the newer build"
        );
        assert!(
            jobs.iter().all(|&pgid| group_alive(pgid)),
            "a job ended: {}",
            fs::read_to_string(env.home.daemon_log()).unwrap()
        );
    }
    drop(conn);

    wait_within(
        "the call's outcome",
        std::time::Duration::from_secs(30),
        || detached_outcome(&env.log(id)).is_some(),
    );
    let outcome = detached_outcome(&env.log(id)).unwrap().to_owned();
    assert!(outcome.contains(r#""exit":7"#), "{outcome}");
    assert!(outcome.contains(r"start\nend"), "{outcome}");
    wait_within("the job's end", std::time::Duration::from_secs(30), || {
        job_ended_holding_a_blob(&env.log(id))
    });
    assert!(
        blob_holds(&env, "now\nlate\n"),
        "the job's output was not kept"
    );

    // The daemon log is written behind the facts it records.
    let log = || fs::read_to_string(env.home.daemon_log()).unwrap_or_default();
    let logged = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !log().contains("\"origin\":\"handover\"") {
        assert!(std::time::Instant::now() < logged, "{}", log());
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let log = log();
    assert!(log.contains("\"handed_over\""), "{log}");
    assert!(!log.contains("previous_crash"), "{log}");

    // The server job runs on, the new daemon's own: stopping it ends it.
    let server = jobs[0];
    assert!(group_alive(server));
    env.cmd(&["daemon", "stop"]).status().unwrap();
    env.wait_exited("the daemon to stop");
    wait_for("the adopted job to be stopped", || !group_alive(server));
}

/// A newer build that declares another handover takes nothing over: the
/// upgrade waits, the older build serving the newer one's clients meanwhile
/// and telling them what holds it, until the job ends; then it makes way.
#[test]
fn an_upgrade_the_newer_build_cannot_take_over_waits_serving_meanwhile() {
    let env = Env::new("handover-refused");
    let old = env.older_build();
    let out = env.cmd_as(&old, &["ls"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pid = env.pid().unwrap();
    let mut conn = env.connect();
    let (id, _) = conn.attach(Target::New, &env.ws);
    conn.input("$ (sleep 6; echo late) & echo now");
    conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    drop(conn);
    let newer = env.home.runtime().stage(&env.exe).unwrap();
    fs::write(newer.exe.with_file_name("handover"), "0").unwrap();

    let out = env.cmd(&["ls"]).output().unwrap();
    let (listed, told) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(listed.starts_with(&id.short().to_string()), "{listed}");
    assert!(told.contains("the upgrade to this build waits"), "{told}");
    assert!(told.contains("`a daemon stop`"), "{told}");
    assert_eq!(env.pid(), Some(pid), "the older daemon made way at once");
    wait_within(
        "the older daemon to make way",
        std::time::Duration::from_secs(30),
        || env.pid() != Some(pid),
    );
    env.run(&["ls"]);
    assert_ne!(env.pid(), Some(pid));
    assert!(job_ended_holding_a_blob(&env.log(id)));
    assert!(blob_holds(&env, "now\nlate\n"));
    let log = fs::read_to_string(env.home.daemon_log()).unwrap();
    assert!(!log.contains("\"handed_over\""), "{log}");
}

/// A daemon started by hand, at a terminal and leading its own process
/// group, hands over where it runs: the newer build stays in its session
/// with its stderr, rather than leaving them as a daemon a client starts
/// does.
#[test]
fn a_daemon_started_by_hand_hands_over_where_it_runs() {
    use std::os::unix::process::CommandExt;
    let env = Env::new("handover-by-hand");
    let old = env.older_build();
    let mut started = env.cmd_as(&old, &["__daemon"]);
    started
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut daemon = started.spawn().unwrap();
    let mut conn = Conn::open_when_listening(&env.home);
    assert!(matches!(conn.hello(), protocol::DaemonMsg::Welcome { .. }));
    conn.attach(Target::New, &env.ws);
    conn.input("$ sleep 30 & echo job");
    conn.until(|e| matches!(e.body, Body::ToolResult { .. }));
    drop(conn);

    env.run(&["ls"]);
    assert_eq!(env.pid(), Some(daemon.id()), "the daemon left its process");
    assert!(daemon.try_wait().unwrap().is_none(), "the daemon ended");
    env.cmd(&["daemon", "stop"]).status().unwrap();
    assert!(daemon.wait().unwrap().success());
}
