use std::path::PathBuf;
use std::time::{Duration, Instant};

use protocol::{
    Activity, Audience, Body, Entry, Environment, Outcome, Residency, SessionId, Target,
};
use serde_json::json;
use store::{Header, Jsonl, LogStore};
use sys::Home;

use ext::host::Registry;

use super::{Config, Daemon};

fn home(name: &str) -> (PathBuf, Home) {
    let root = std::env::temp_dir().join(format!("a-engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    (root.clone(), Home::from(root))
}

/// Every extension in the repository, as `a` links them, with `policy`,
/// and the fixtures.
pub(crate) fn extensions(policy: &ext_policy::Policy) -> Result<Registry, ext::Refused> {
    Registry::default()
        .with_in(ext::Tier::Guarded, policy)?
        .with_in(ext::Tier::Pre, &ext_prompt::Prompt)?
        .with_in(ext::Tier::Pre, &ext_shell::Shell)?
        .with_in(ext::Tier::Pre, &ext_edit::Edit)?
        .with_in(ext::Tier::Pre, &ext_chat::Chat)?
        .with_in(ext::Tier::Pre, &ext_subagent::Subagent)?
        .with_in(ext::Tier::Pre, &ext_fixtures::Fixtures)
}

/// [`extensions`], configured as `config` says.
pub(crate) fn configured(config: &Config) -> Result<Registry, ext::Refused> {
    extensions(&ext_policy::Policy {
        ask: config.ask.clone(),
    })
}

fn echo() -> Config {
    Config {
        agent: Some(ext_fixtures::ECHO.into()),
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
    let entries = store.entries(id).unwrap();
    entries.into_iter().map(|e| e.body).collect()
}

#[test]
fn idle_sessions_hibernate_and_reload_on_attach() {
    let (root, home) = home("hibernate");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let attachment = daemon.resolve(&Target::New, || "/w".into()).unwrap();
    let id = attachment.session().id;
    attachment.session().submit("hi".into()).unwrap();
    settle(&daemon);
    drop(attachment);

    daemon.hibernate(Duration::ZERO);
    let hibernated = daemon.slots()[0].residence.lock().unwrap().live().is_none();
    let listed = daemon.list();
    let again = daemon
        .resolve(&Target::Id(id), || "/elsewhere".into())
        .unwrap();
    let reloaded = again.session().summary().entries;
    let mine = Target::Latest {
        any_workspace: false,
    };
    let latest = daemon.resolve(&mine, || "/w".into());
    let missing = daemon.resolve(&mine, || "/x".into());
    // The reloaded session's recovery pass touches its busy marker.
    settle(&daemon);
    std::fs::remove_dir_all(&root).unwrap();

    assert!(hibernated);
    assert_eq!((listed.len(), listed[0].entries), (1, 2));
    assert_eq!(reloaded, 2);
    assert_eq!(latest.unwrap().session().id, id);
    assert!(missing.is_err());
}

/// A hibernated session's log is counted once, and counted again after it
/// was loaded and grew.
#[test]
fn a_hibernated_session_lists_the_entries_it_has_now() {
    let (root, home) = home("count");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let mut counts = Vec::new();
    let mut target = Target::New;
    for text in ["one", "two"] {
        let attachment = daemon.resolve(&target, || "/w".into()).unwrap();
        target = Target::Id(attachment.session().id);
        attachment.session().submit(text.into()).unwrap();
        settle(&daemon);
        drop(attachment);
        daemon.hibernate(Duration::ZERO);
        let Target::Id(id) = target else {
            unreachable!()
        };
        let stored = daemon.shared.store.count(id).unwrap();
        counts.push([daemon.list()[0].entries, daemon.list()[0].entries, stored]);
    }
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(counts[0], [2; 3]);
    assert!(
        counts[1][2] > 2 && counts[1] == [counts[1][2]; 3],
        "{counts:?}"
    );
}

#[test]
fn busy_sessions_recover_at_startup() {
    let (root, home) = home("recover");
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let id = SessionId::new();
    let mut created = store
        .create(&Header {
            id,
            workspace: "/w".into(),
            parent: None,
            // As an earlier version wrote it.
            extensions: Vec::new(),
        })
        .unwrap();
    let calls = [
        ("sleep", json!({ "seconds": 0 })),
        ("run_command", json!({ "command": "true" })),
    ];
    for (seq, (tool, input)) in (1..).zip(calls) {
        let body = Body::ToolCall {
            tool: tool.into(),
            input: input.into(),
            id: None,
            card: None,
            by: Audience::Model,
        };
        created
            .log
            .append(&Entry::new(seq, created.epoch, body))
            .unwrap();
    }
    store.mark_busy(id, true).unwrap();

    let daemon = Daemon::load(&home, &echo()).unwrap();
    settle(&daemon);
    // Its actor clears the marker after the work it settles.
    drop(daemon);
    let got = bodies(&store, id);
    let still_busy = store.is_busy(id);
    std::fs::remove_dir_all(&root).unwrap();

    // The log's header predates extension records, so recovery notes them
    // first. The safe sleep runs again; the shell command is not rerun.
    assert!(matches!(&got[2], Body::Extensions { installed } if installed.len() == 7));
    assert!(matches!(
        &got[3],
        Body::ToolResult { call: 1, outcome: Outcome::Done { output }, card: None } if output.value()["slept"] == 0
    ));
    assert!(matches!(
        &got[4],
        Body::ToolResult {
            call: 2,
            outcome: Outcome::Interrupted { output: None },
            card: None,
        }
    ));
    assert_eq!(got.len(), 5);
    assert!(!still_busy);
}

/// A session waiting for approval is listed as such once hibernated, and
/// a restarted daemon loads it at once to ask again.
#[test]
fn a_session_waiting_for_approval_stays_waiting_through_hibernation_and_a_restart() {
    let (root, home) = home("asked");
    let config = Config {
        ask: vec!["bash".into()],
        ..echo()
    };
    let daemon = Daemon::load(&home, &config).unwrap();
    let attachment = daemon.resolve(&Target::New, || "/w".into()).unwrap();
    let id = attachment.session().id;
    attachment.session().submit("$ echo hi".into()).unwrap();
    settle(&daemon);
    drop(attachment);
    daemon.hibernate(Duration::ZERO);
    let hibernated = daemon.list().remove(0);
    drop(daemon);
    let marked = Jsonl::open(home.sessions_dir()).unwrap().is_busy(id);

    let restarted = Daemon::load(&home, &config).unwrap();
    let listed = restarted.list().remove(0);
    let asked = restarted.slots()[0]
        .residence
        .lock()
        .unwrap()
        .live()
        .map(|s| s.summary().activity);
    drop(restarted);
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(
        (hibernated.activity, hibernated.residency),
        (Activity::Approval, Residency::Hibernated)
    );
    assert_eq!(
        (listed.activity, listed.residency),
        (Activity::Approval, Residency::Loaded)
    );
    assert!(marked, "a session still asking stays marked");
    assert_eq!(asked, Some(Activity::Approval));
}

/// A daemon whose `bash` calls wait for approval.
fn asking(name: &str) -> (PathBuf, Home, super::Owner) {
    let (root, home) = home(name);
    let config = Config {
        ask: vec!["bash".into()],
        ..echo()
    };
    let daemon = Daemon::load(&home, &config).unwrap();
    (root, home, daemon)
}

#[test]
fn no_writer_outlives_its_daemon() {
    let (root, home, daemon) = asking("outlives");
    let waiting = daemon.resolve(&Target::New, || "/w".into()).unwrap();
    waiting.session().submit("$ echo hi".into()).unwrap();
    let idle = daemon.resolve(&Target::New, || "/w".into()).unwrap();
    settle(&daemon);
    let ended = [waiting.session().ended(), idle.session().ended()];
    let id = waiting.session().id;
    drop(daemon);

    let over = ended.iter().map(super::Ended::is_over).collect::<Vec<_>>();
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let at_exit = store.count(id).unwrap();
    // A client still holding on cannot write either: the log is closed.
    let refused = waiting.session().submit("more".into());
    let after = store.count(id).unwrap();
    drop((waiting, idle));
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(over, [true, true]);
    assert!(refused.is_err());
    assert_eq!(after, at_exit);
}

#[test]
fn a_hibernated_session_reloads_only_after_its_actor_has_exited() {
    let (root, _, daemon) = asking("reload");
    let attachment = daemon.resolve(&Target::New, || "/w".into()).unwrap();
    let id = attachment.session().id;
    attachment.session().submit("$ echo hi".into()).unwrap();
    settle(&daemon);
    let first = attachment.session().ended();
    drop(attachment);

    // Hibernated while its actor still waits for the answer, then wanted
    // at once.
    daemon.hibernate(Duration::ZERO);
    let again = daemon.resolve(&Target::Id(id), || "/w".into()).unwrap();
    let over = first.is_over();
    let asks = again.session().summary().activity;
    drop(again);
    drop(daemon);
    std::fs::remove_dir_all(&root).unwrap();

    assert!(over, "loaded while the last actor ran on");
    assert_eq!(asks, Activity::Approval);
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
    let parent = daemon.resolve(&Target::New, || "/w".into()).unwrap();
    let id = parent.session().id;
    parent.session().submit("spawn hi".into()).unwrap();
    settle(&daemon);
    drop(parent);
    let store = &*daemon.shared.store;
    let got = bodies(store, id);
    let Body::Link { child, .. } = got[2] else {
        panic!("{got:?}")
    };
    let child_header = store.headers().unwrap().into_iter().find(|h| h.id == child);
    let child_log = bodies(store, child);
    let latest = Target::Latest {
        any_workspace: false,
    };
    let resumed = daemon
        .resolve(&latest, || "/w".into())
        .unwrap()
        .session()
        .id;
    settle(&daemon);
    // The child working again reports nothing: the parent's watch fired,
    // so the hibernated parent stays unloaded.
    daemon.hibernate(Duration::ZERO);
    let again = daemon.resolve(&Target::Id(child), || "/w".into()).unwrap();
    again.session().submit("more".into()).unwrap();
    settle(&daemon);
    drop(again);
    let parent_slot = daemon.slots().into_iter().find(|s| s.header.id == id);
    let parent_loaded = parent_slot
        .unwrap()
        .residence
        .lock()
        .unwrap()
        .live()
        .is_some();
    drop(daemon);

    // A restart finds nothing left to fire.
    let again = Daemon::load(&home, &echo()).unwrap();
    settle(&again);
    let after = bodies(&*again.shared.store, id);
    std::fs::remove_dir_all(&root).unwrap();

    // The `task` call waits on the child it linked, and answers with what
    // the child settled with.
    assert_eq!(
        got[2..],
        [
            Body::Link {
                child,
                input: "hi".into()
            },
            Body::Wait {
                call: protocol::CallId::new(2).unwrap(),
                on: protocol::WaitOn::Child {
                    link: protocol::Seq::new(3).unwrap()
                }
            },
            Body::Watch { child },
            Body::Fired {
                watch: 5,
                outcome: Outcome::Done {
                    output: "hi".into()
                }
            },
            Body::ToolResult {
                call: 2,
                outcome: Outcome::Done {
                    output: "The child session finished:\nhi".into()
                },
                card: None
            },
        ]
    );
    assert!(matches!(&got[1], Body::ToolCall { tool, .. } if tool == "task"));
    assert!(!parent_loaded, "a moot report loaded the parent");
    assert_eq!(child_header.unwrap().parent.unwrap().call, 3);
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
    let parent = daemon.resolve(&Target::New, || "/w".into()).unwrap();
    let id = parent.session().id;
    let vars = [("PATH", "/venv/bin"), ("API_TOKEN", "s3cret")];
    let env = Environment {
        cwd: "/w/sub".into(),
        vars: vars.map(|(k, v)| (k.into(), v.into())).into(),
        ..Environment::default()
    };
    parent.session().set_env(env.clone()).unwrap();
    parent.session().submit("spawn hi".into()).unwrap();
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
        let mut created = store.create(header).unwrap();
        for (seq, body) in (1..).zip(bodies) {
            let entry = Entry::new(seq, created.epoch, body);
            created.log.append(&entry).unwrap();
        }
        store.mark_busy(header.id, busy).unwrap();
    };
    let session = |parent: Option<SessionId>| Header {
        id: SessionId::new(),
        workspace: "/w".into(),
        parent: parent.map(|session| store::Parent { session, call: 3 }),
        extensions: Vec::new(),
    };
    // The `task` call, its link, its wait on the child and the watch commit
    // in one batch.
    let spawned = |child: SessionId| {
        vec![
            Body::User {
                text: "spawn sleep 0".into(),
            },
            Body::ToolCall {
                tool: "task".into(),
                input: json!({ "prompt": "sleep 0" }).into(),
                id: None,
                card: None,
                by: Audience::Model,
            },
            Body::Link {
                child,
                input: "sleep 0".into(),
            },
            Body::Wait {
                call: protocol::CallId::new(2).unwrap(),
                on: protocol::WaitOn::Child {
                    link: protocol::Seq::new(3).unwrap(),
                },
            },
            Body::Watch { child },
        ]
    };
    let opened = || Body::User {
        text: "sleep 0".into(),
    };
    let sleep = || Body::ToolCall {
        tool: "sleep".into(),
        input: json!({ "seconds": 0 }).into(),
        id: None,
        card: None,
        by: Audience::Model,
    };
    let slept = Outcome::Done {
        output: json!({ "slept": 0 }).into(),
    };
    let mut parents = Vec::new();

    // Killed while the child ran.
    let parent = session(None);
    let child = session(Some(parent.id));
    write(&parent, spawned(child.id), true);
    write(&child, vec![opened(), sleep()], true);
    parents.push(parent.id);
    // Killed after the child finished, before the parent heard.
    let parent = session(None);
    let child = session(Some(parent.id));
    write(&parent, spawned(child.id), true);
    let done = Body::ToolResult {
        call: 2,
        outcome: slept.clone(),
        card: None,
    };
    write(&child, vec![opened(), sleep(), done], false);
    parents.push(parent.id);
    // Killed before the child was created.
    let parent = session(None);
    write(&parent, spawned(SessionId::new()), true);
    parents.push(parent.id);

    for _ in 0..2 {
        let daemon = Daemon::load(&home, &echo()).unwrap();
        settle(&daemon);
    }
    let got: Vec<_> = parents.iter().map(|&id| bodies(&store, id)).collect();
    std::fs::remove_dir_all(&root).unwrap();

    for log in &got {
        assert_eq!(fired(log), [&slept], "{log:#?}");
        // Run again, the call waited on the child it had, starting no other.
        let links = log.iter().filter(|b| matches!(b, Body::Link { .. }));
        assert_eq!(links.count(), 1, "{log:#?}");
        assert!(
            matches!(log.last(), Some(Body::ToolResult { call: 2, .. })),
            "{log:#?}"
        );
    }
}

/// Waits until `done` holds for session `id`'s committed log.
fn until(daemon: &Daemon, id: SessionId, done: impl Fn(&[Body]) -> bool) -> Vec<Body> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let log = bodies(&*daemon.shared.store, id);
        if done(&log) {
            return log;
        }
        assert!(Instant::now() < deadline, "never happened: {log:#?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn an_owner_idles_with_its_children_and_an_interrupt_stops_them_first() {
    let (root, home) = home("owned");
    std::fs::create_dir_all(&root).unwrap();
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let parent = daemon.resolve(&Target::New, || root.clone()).unwrap();
    let id = parent.session().id;
    parent.session().submit("spawn $ sleep 100".into()).unwrap();
    drop(parent);
    let slot = daemon.slots().into_iter().find(|s| s.header.id == id);
    let session = slot
        .unwrap()
        .residence
        .lock()
        .unwrap()
        .live()
        .cloned()
        .unwrap();
    let log = until(&daemon, id, |log| {
        log.iter().any(|b| matches!(b, Body::Watch { .. }))
    });
    let Some(Body::Link { child, .. }) = log.get(2) else {
        panic!("{log:#?}")
    };
    until(&daemon, *child, |log| {
        log.iter().any(|b| matches!(b, Body::ToolProcess { .. }))
    });
    let owned = daemon.children(id);
    let busy = session.idle_for();
    session.interrupt();
    settle(&daemon);
    let child_log = bodies(&*daemon.shared.store, *child);
    let parent_log = bodies(&*daemon.shared.store, id);
    daemon.hibernate(Duration::ZERO);
    let unloaded = daemon.children(id).is_empty();
    drop(daemon);
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(owned.len(), 1);
    assert_eq!(busy, Duration::ZERO);
    let aborted = |b: &Body| matches!(b, Body::ToolResult { outcome: Outcome::Done { output }, .. } if output.value()["aborted"] == true);
    assert!(child_log.iter().any(aborted), "{child_log:#?}");
    // The child settles first, and its parent's call hears how.
    let heard = |b: &Body| matches!(b, Body::ToolResult { outcome: Outcome::Done { output }, .. } if output.value().as_str().is_some_and(|t| t.contains("aborted")));
    let tail: Vec<_> = parent_log.iter().rev().take(2).collect();
    assert!(
        matches!(tail[..], [result, Body::Fired { .. }] if heard(result)),
        "{parent_log:#?}"
    );
    assert!(unloaded);
}

#[test]
fn an_orphan_does_not_resume_what_it_was_doing() {
    let (root, home) = home("orphan");
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let header = Header {
        id: SessionId::new(),
        workspace: "/w".into(),
        parent: Some(store::Parent {
            session: SessionId::new(),
            call: 2,
        }),
        extensions: Vec::new(),
    };
    let mut created = store.create(&header).unwrap();
    let bodies_written = [
        Body::User {
            text: "sleep 0".into(),
        },
        Body::ToolCall {
            tool: "sleep".into(),
            input: json!({ "seconds": 0 }).into(),
            id: None,
            card: None,
            by: Audience::Model,
        },
    ];
    for (seq, body) in (1..).zip(bodies_written) {
        let entry = Entry::new(seq, created.epoch, body);
        created.log.append(&entry).unwrap();
    }
    store.mark_busy(header.id, true).unwrap();
    drop(created);

    let daemon = Daemon::load(&home, &echo()).unwrap();
    settle(&daemon);
    let got = bodies(&store, header.id);
    drop(daemon);
    std::fs::remove_dir_all(&root).unwrap();

    // The safe sleep would run again in a session whose owner is there.
    assert!(
        matches!(
            &got[2..],
            [
                Body::Extensions { .. },
                Body::ToolResult {
                    call: 2,
                    outcome: Outcome::Interrupted { output: None },
                    ..
                },
            ]
        ),
        "{got:#?}"
    );
}

/// Keeps every event the daemon emits in tests.
pub struct Recorder(std::sync::Mutex<Vec<rt::Event>>);

impl Recorder {
    pub const fn new() -> Self {
        Self(std::sync::Mutex::new(Vec::new()))
    }

    fn count(&self, name: &str) -> usize {
        crate::lock(&self.0)
            .iter()
            .filter(|e| e.name == name)
            .count()
    }

    fn of(&self, session: SessionId) -> Vec<rt::Event> {
        let session = session.to_string();
        crate::lock(&self.0)
            .iter()
            .filter(|e| e.session.as_ref() == Some(&session))
            .cloned()
            .collect()
    }
}

impl rt::EventSink for Recorder {
    const ENABLED: bool = true;

    fn emit(&self, event: impl FnOnce() -> rt::Event) {
        crate::lock(&self.0).push(event());
    }
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_supervisor_sleeps_until_a_deadline_and_never_polls() {
    use super::{EVENTS, Exit, Lifecycle};

    let (root, home) = home("supervise");
    std::fs::create_dir_all(&root).unwrap();
    let config = Config {
        idle: Duration::from_secs(3600),
        hibernate: Duration::from_millis(300),
        ..echo()
    };
    let daemon = Daemon::load(&home, &config).unwrap();
    let exit = Exit {
        home: home.clone(),
        pid: std::fs::File::create(root.join("pid")).unwrap(),
        log: None,
    };
    let life = Lifecycle::new("test".into(), home.runtime());
    let supervisor = std::sync::Arc::clone(&daemon);
    daemon.shared.rt.spawn(Box::pin(async move {
        supervisor.supervise(&life, &config, exit).await;
    }));
    wait_until("the first look", || EVENTS.count("wake") > 0);

    let attachment = daemon.resolve(&Target::New, || "/w".into()).unwrap();
    let id = attachment.session().id;
    attachment.session().submit("$ true".into()).unwrap();
    settle(&daemon);
    let detached = Instant::now();
    drop(attachment);
    let hibernated = || daemon.slots()[0].residence.lock().unwrap().live().is_none();
    wait_until("hibernation", hibernated);
    let after = detached.elapsed();

    // Nothing is due for an hour: nothing wakes the supervisor.
    let wakes = EVENTS.count("wake");
    std::thread::sleep(Duration::from_secs(1));
    let idle_wakes = EVENTS.count("wake") - wakes;
    let names: Vec<_> = EVENTS.of(id).iter().map(|e| e.name).collect();
    std::fs::remove_dir_all(&root).unwrap();

    assert!(
        after >= Duration::from_millis(300),
        "hibernated after {after:?}"
    );
    assert_eq!(idle_wakes, 0);
    assert!(wakes < 20, "{wakes} wakes for one turn");
    assert_eq!(names, ["effect", "step", "hibernate"]);
}

#[test]
fn a_daemon_after_a_crash_loop_refuses_to_serve_until_reset() {
    use super::{Claim, claim};

    let (root, home) = home("breaker");
    std::fs::create_dir_all(home.run_dir()).unwrap();
    std::fs::write(home.run_dir().join("crashes"), "3\n").unwrap();
    let Claim::Tripped(why) = claim(&home, "b1", None, false, 3).unwrap() else {
        panic!("served after a crash loop");
    };
    assert!(
        why.contains("crashed 3 times") && why.contains("a daemon restart"),
        "{why}"
    );
    assert!(matches!(
        claim(&home, "b1", None, false, 0).unwrap(),
        Claim::Serve(_)
    ));
    // That daemon exited cleanly.
    std::fs::write(home.daemon_pid_file(), "").unwrap();
    assert!(matches!(
        claim(&home, "b1", None, true, 3).unwrap(),
        Claim::Serve(_)
    ));
    assert_eq!(sys::crash::Breaker::of(&home).young(), 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn watchers_are_sent_the_session_list_as_it_changes() {
    use super::outbox::Outbox;
    use super::{Exit, Lifecycle};
    use protocol::{Caps, DaemonMsg, SessionSummary};

    let (root, home) = home("watch");
    std::fs::create_dir_all(&root).unwrap();
    let config = Config {
        idle: Duration::from_secs(3600),
        ..echo()
    };
    let daemon = Daemon::load(&home, &config).unwrap();
    let exit = Exit {
        home: home.clone(),
        pid: std::fs::File::create(root.join("pid")).unwrap(),
        log: None,
    };
    let life = Lifecycle::new("test".into(), home.runtime());
    let supervisor = std::sync::Arc::clone(&daemon);
    daemon.shared.rt.spawn(Box::pin(async move {
        supervisor.supervise(&life, &config, exit).await;
    }));

    let watcher = Outbox::new(Caps::ALL);
    daemon.watch_list(&watcher);
    let seen = std::sync::Mutex::new(Vec::<Vec<SessionSummary>>::new());
    let latest = |pick: &dyn Fn(&[SessionSummary]) -> bool| {
        let mut seen = seen.lock().unwrap();
        for frame in std::iter::from_fn(|| watcher.try_next()) {
            if let Ok(Some(DaemonMsg::Sessions { sessions })) =
                protocol::read_frame::<DaemonMsg>(&mut frame.as_bytes())
            {
                seen.push(sessions);
            }
        }
        seen.last().is_some_and(|list| pick(list))
    };
    assert!(
        latest(&|list| list.is_empty()),
        "the list as it is, at once"
    );

    let attachment = daemon.resolve(&Target::New, || "/w".into()).unwrap();
    wait_until("the new session listed", || {
        latest(&|list| list.len() == 1 && list[0].clients == 1)
    });
    drop(attachment);
    wait_until("its client gone", || {
        latest(&|list| list.len() == 1 && list[0].clients == 0)
    });
    // Only changes are sent.
    let sent = seen.lock().unwrap().len();
    std::thread::sleep(Duration::from_millis(100));
    latest(&|_| true);
    assert_eq!(seen.lock().unwrap().len(), sent);
    settle(&daemon);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_session_is_found_by_any_unambiguous_prefix_of_its_id() {
    let (root, home) = home("prefix");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let ids: Vec<SessionId> = (0..2)
        .map(|_| {
            daemon
                .resolve(&Target::New, || "/w".into())
                .unwrap()
                .session()
                .id
        })
        .collect();
    let found = |prefix: String| {
        daemon
            .resolve(&Target::Prefix(prefix), || "/w".into())
            .map(|a| a.session().id)
    };
    assert_eq!(found(ids[0].to_string()).unwrap(), ids[0]);
    assert_eq!(found(ids[1].short().to_string()).unwrap(), ids[1]);
    // UUIDv7s made together share their first digits.
    let several = found(ids[0].to_string()[..4].to_owned()).unwrap_err();
    assert!(several.to_string().contains("several"), "{several}");
    let none = found("zz".into()).unwrap_err();
    assert_eq!(none.kind(), std::io::ErrorKind::NotFound);
    settle(&daemon);
    std::fs::remove_dir_all(&root).unwrap();
}
