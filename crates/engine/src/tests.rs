use std::path::PathBuf;
use std::time::{Duration, Instant};

use protocol::{Body, Entry, Environment, Outcome, SessionId, Target};
use serde_json::json;
use store::{Header, Jsonl, LogStore};
use sys::Home;

use super::{Config, Daemon};

fn home(name: &str) -> (PathBuf, Home) {
    let root = std::env::temp_dir().join(format!("a-engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    (root.clone(), Home::from(root))
}

fn echo() -> Config {
    Config {
        agent: agent::Kind::Echo,
        ..Config::default()
    }
}

fn settle(daemon: &Daemon) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon.idle() {
        assert!(Instant::now() < deadline, "work never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn bodies(store: &dyn LogStore, id: SessionId) -> Vec<Body> {
    let loaded = store.load(id).unwrap();
    loaded.entries.into_iter().map(|e| e.body).collect()
}

#[test]
fn idle_sessions_hibernate_and_reload_on_attach() {
    let (root, home) = home("hibernate");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let attachment = daemon.resolve(&Target::New, "/w".into()).unwrap();
    let id = attachment.session().id;
    attachment.session().submit("hi".into());
    settle(&daemon);
    drop(attachment);

    daemon.hibernate(Duration::ZERO);
    let hibernated = daemon.slots()[0].live.lock().unwrap().is_none();
    let listed = daemon.list();
    let again = daemon
        .resolve(&Target::Id(id), "/elsewhere".into())
        .unwrap();
    let reloaded = again.session().summary().entries;
    let latest = daemon.resolve(
        &Target::Latest {
            any_workspace: false,
        },
        "/w".into(),
    );
    let missing = daemon.resolve(
        &Target::Latest {
            any_workspace: false,
        },
        "/x".into(),
    );
    // The reloaded session's recovery pass touches its busy marker.
    settle(&daemon);
    std::fs::remove_dir_all(&root).unwrap();

    assert!(hibernated);
    assert_eq!((listed.len(), listed[0].entries), (1, 2));
    assert_eq!(reloaded, 2);
    assert_eq!(latest.unwrap().session().id, id);
    assert!(missing.is_err());
}

#[test]
fn busy_sessions_recover_at_startup() {
    let (root, home) = home("recover");
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let id = SessionId::new();
    let mut log = store
        .create(&Header {
            id,
            workspace: "/w".into(),
            parent: None,
        })
        .unwrap()
        .log;
    let calls = [
        ("sleep", json!({ "seconds": 0 })),
        ("run_command", json!({ "command": "true" })),
    ];
    for (seq, (tool, input)) in (1..).zip(calls) {
        let body = Body::ToolCall {
            tool: tool.into(),
            input,
            id: None,
        };
        log.append(&Entry {
            seq,
            epoch: 0,
            body,
        })
        .unwrap();
    }
    store.mark_busy(id, true).unwrap();

    let daemon = Daemon::load(&home, &echo()).unwrap();
    settle(&daemon);
    let got = bodies(&store, id);
    let still_busy = store.is_busy(id);
    std::fs::remove_dir_all(&root).unwrap();

    // The safe sleep runs again; the shell command is not rerun.
    assert!(matches!(
        &got[2],
        Body::ToolResult { call: 1, outcome: Outcome::Done { output } } if output["slept"] == 0
    ));
    assert!(matches!(
        &got[3],
        Body::ToolResult {
            call: 2,
            outcome: Outcome::Interrupted
        }
    ));
    assert_eq!(got.len(), 4);
    assert!(!still_busy);
}

fn fired(bodies: &[Body]) -> Vec<&Outcome> {
    bodies
        .iter()
        .filter_map(|b| match b {
            Body::Fired { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect()
}

#[test]
fn a_parent_hears_from_its_child_exactly_once() {
    let (root, home) = home("links");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let parent = daemon.resolve(&Target::New, "/w".into()).unwrap();
    let id = parent.session().id;
    parent.session().submit("spawn hi".into());
    settle(&daemon);
    drop(parent);
    let store = &*daemon.shared.store;
    let got = bodies(store, id);
    let Body::Link { child, .. } = got[1] else {
        panic!("{got:?}")
    };
    let child_header = store.headers().unwrap().into_iter().find(|h| h.id == child);
    let child_log = bodies(store, child);
    let latest = Target::Latest {
        any_workspace: false,
    };
    let resumed = daemon.resolve(&latest, "/w".into()).unwrap().session().id;
    settle(&daemon);
    drop(daemon);

    // A restart finds nothing left to fire.
    let again = Daemon::load(&home, &echo()).unwrap();
    settle(&again);
    let after = bodies(&*again.shared.store, id);
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(
        got[1..],
        [
            Body::Link {
                child,
                input: "hi".into()
            },
            Body::Watch { child },
            Body::Fired {
                watch: 3,
                outcome: Outcome::Done {
                    output: "hi".into()
                }
            },
        ]
    );
    assert_eq!(child_header.unwrap().parent.unwrap().call, 2);
    assert_eq!(resumed, id, "resuming picks the parent, not its child");
    assert_eq!(
        child_log,
        [
            Body::User { text: "hi".into() },
            Body::Assistant { text: "hi".into() },
        ]
    );
    assert_eq!(after, got);
}

#[test]
fn a_child_runs_in_its_parents_environment() {
    let (root, home) = home("child-env");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let parent = daemon.resolve(&Target::New, "/w".into()).unwrap();
    let id = parent.session().id;
    let vars = [("PATH", "/venv/bin"), ("API_TOKEN", "s3cret")];
    let env = Environment {
        cwd: "/w/sub".into(),
        vars: vars.map(|(k, v)| (k.into(), v.into())).into(),
        ..Environment::default()
    };
    parent.session().set_env(env.clone()).unwrap();
    parent.session().submit("spawn hi".into());
    settle(&daemon);
    drop(parent);
    let store = &*daemon.shared.store;
    let child = bodies(store, id).into_iter().find_map(|b| match b {
        Body::Link { child, .. } => Some(child),
        _ => None,
    });
    let opening = bodies(store, child.unwrap()).into_iter().next();
    drop(daemon);
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(opening, Some(Body::Environment(env.redacted())));
}

/// Logs as a daemon killed at various points leaves them: each parent's
/// watch fires once the next daemon has recovered, and never again.
#[test]
fn watches_rearm_after_a_crash_and_fire_once() {
    let (root, home) = home("rearm");
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let write = |header: &Header, bodies: Vec<Body>, busy: bool| {
        let mut log = store.create(header).unwrap().log;
        for (seq, body) in (1..).zip(bodies) {
            log.append(&Entry {
                seq,
                epoch: 0,
                body,
            })
            .unwrap();
        }
        store.mark_busy(header.id, busy).unwrap();
    };
    let session = |parent: Option<SessionId>| Header {
        id: SessionId::new(),
        workspace: "/w".into(),
        parent: parent.map(|session| store::Parent { session, call: 2 }),
    };
    let spawned = |child: SessionId, watched: bool| {
        let mut bodies = vec![
            Body::User {
                text: "spawn sleep 0".into(),
            },
            Body::Link {
                child,
                input: "sleep 0".into(),
            },
        ];
        if watched {
            bodies.push(Body::Watch { child });
        }
        bodies
    };
    let opened = || Body::User {
        text: "sleep 0".into(),
    };
    let sleep = || Body::ToolCall {
        tool: "sleep".into(),
        input: json!({ "seconds": 0 }),
        id: None,
    };
    let slept = Outcome::Done {
        output: json!({ "slept": 0 }),
    };
    let mut parents = Vec::new();

    // Killed while the child ran.
    let parent = session(None);
    let child = session(Some(parent.id));
    write(&parent, spawned(child.id, true), true);
    write(&child, vec![opened(), sleep()], true);
    parents.push(parent.id);
    // Killed after the child finished, before the parent heard.
    let parent = session(None);
    let child = session(Some(parent.id));
    write(&parent, spawned(child.id, true), true);
    let done = Body::ToolResult {
        call: 2,
        outcome: slept.clone(),
    };
    write(&child, vec![opened(), sleep(), done], false);
    parents.push(parent.id);
    // Killed between the link and the child's creation.
    let parent = session(None);
    write(&parent, spawned(SessionId::new(), false), true);
    parents.push(parent.id);

    for _ in 0..2 {
        let daemon = Daemon::load(&home, &echo()).unwrap();
        settle(&daemon);
    }
    let got: Vec<_> = parents.iter().map(|&id| bodies(&store, id)).collect();
    std::fs::remove_dir_all(&root).unwrap();

    for log in &got {
        assert_eq!(fired(log), [&slept], "{log:#?}");
    }
}
