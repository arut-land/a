use std::path::PathBuf;
use std::time::{Duration, Instant};

use protocol::{Body, Entry, Outcome, SessionId, Target};
use serde_json::json;
use store::{Header, Store};
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

fn bodies(store: &Store, id: SessionId) -> Vec<Body> {
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
    let store = Store::open(home.sessions_dir()).unwrap();
    let id = SessionId::new();
    let mut log = store
        .create(&Header {
            id,
            workspace: "/w".into(),
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
