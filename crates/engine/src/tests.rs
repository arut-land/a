use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::store::{Header, Jsonl, LogStore};
use protocol::ledger::Ledger;
use protocol::{
    Activity, Audience, Body, CallId, Entry, Environment, Epoch, Json, Outcome, Residency, Seq,
    SessionId, Target,
};
use serde_json::json;
use sys::Home;

use ext::host::Registry;

use super::{Attachment, Config, Daemon, Live};

impl Daemon {
    /// [`Daemon::resolve`], from a test's thread.
    pub(crate) fn attach_to(
        &self,
        target: &Target,
        workspace: impl FnOnce() -> PathBuf,
    ) -> std::io::Result<Attachment> {
        rt::block_on(self.resolve(target, workspace))
    }

    /// The loaded sessions `parent` owns.
    fn children(&self, parent: SessionId) -> Vec<std::sync::Arc<Live>> {
        (self.slots().into_iter())
            .filter(|s| s.header.parent.is_some_and(|p| p.session == parent))
            .filter_map(|s| s.live())
            .collect()
    }
}

fn home(name: &str) -> (PathBuf, Home) {
    let root = std::env::temp_dir().join(format!("a-engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    (root.clone(), Home::from(root))
}

/// Every extension in the repository, as `a` links them, with `policy`,
/// and the fixtures.
pub(crate) fn extensions(policy: &ext_policy::Policy) -> Result<Registry, ext::Refused> {
    Registry::default()
        .with_in(ext::Tier::Pre, &ext_fork::Fork)?
        .with_in(ext::Tier::Pre, policy)?
        .with_in(ext::Tier::Pre, &ext_prompt::Prompt::default())?
        .with_in(ext::Tier::Pre, &ext_shell::Shell)?
        .with_in(ext::Tier::Pre, &ext_edit::Edit)?
        .with_in(ext::Tier::Pre, &ext_subagent::Subagent)?
        .with_in(ext::Tier::Pre, &ext_fixtures::Fixtures)
}

/// [`extensions`], with the tools `config`'s `[ext.policy]` asks about.
pub(crate) fn configured(config: &Config) -> Result<Registry, ext::Refused> {
    let policy = config.ext.table("policy");
    let ask = policy.list("ask").ok().flatten().unwrap_or_default();
    extensions(&ext_policy::Policy {
        ask: ask.to_vec(),
        ..ext_policy::Policy::default()
    })
}

/// The `[ext]` tables of `text`.
fn exts(text: &str) -> sys::config::Exts {
    text.parse::<sys::Config>().unwrap().ext
}

fn echo() -> Config {
    Config {
        agent: Some(ext_fixtures::ECHO.into()),
        ..Config::default()
    }
}

/// How long a test waits for the daemon before giving up.
const PATIENCE: Duration = Duration::from_secs(10);

/// Waits until every loaded session has taken what it was sent and has no
/// work.
fn settle(daemon: &Daemon) {
    for live in daemon.loaded() {
        live.barrier();
    }
    daemon.wait("settled", PATIENCE, || daemon.idle());
}

impl Daemon {
    /// Returns once `ready` holds, checked as the daemon's sessions change,
    /// as jobs end and as events are emitted; panics with `what` if it
    /// does not within `within`.
    pub(crate) fn wait(&self, what: &str, within: Duration, ready: impl Fn() -> bool + Sync) {
        let timeout = rt::Reactor::AsyncIo.sleep(within);
        let held = rt::block_on(rt::or(
            async {
                let news = rt::or(self.pulse.until(&ready), self.jobs.until(&ready));
                rt::or(news, super::EVENTS.1.until(&ready)).await;
                true
            },
            async {
                timeout.await;
                false
            },
        ));
        assert!(held, "never {what}");
    }
}

/// Whether anything happened in the daemon within `within`: an event
/// emitted, a session changed.
fn anything_within(daemon: &Daemon, within: Duration) -> bool {
    let (mut pulsed, mut events) = (daemon.pulse.count(), super::EVENTS.1.count());
    let timeout = rt::Reactor::AsyncIo.sleep(within);
    rt::block_on(rt::or(
        async {
            rt::or(
                daemon.pulse.changed(&mut pulsed),
                super::EVENTS.1.changed(&mut events),
            )
            .await;
            true
        },
        async {
            timeout.await;
            false
        },
    ))
}

/// Whether the catalog marks session `id` busy, as the next daemon reads it.
fn marked(home: &Home, id: SessionId) -> bool {
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let catalog = Catalog::open(home.catalog_file(), &store).unwrap();
    catalog.get(id).is_some_and(|r| r.busy)
}

/// Session `id`'s log; none if it is not made yet.
fn bodies(store: &dyn LogStore, id: SessionId) -> Vec<Body> {
    let entries = store.entries(id).unwrap_or_default();
    entries.into_iter().map(|e| e.body).collect()
}

#[test]
fn idle_sessions_hibernate_and_reload_on_attach() {
    let (root, home) = home("hibernate");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let attachment = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    let id = attachment.session().id;
    attachment.session().submit("hi").unwrap();
    settle(&daemon);
    drop(attachment);

    daemon.hibernate(Duration::ZERO);
    let hibernated = daemon.slots()[0].live().is_none();
    let listed = daemon.list();
    let again = daemon
        .attach_to(&Target::Id(id), || "/elsewhere".into())
        .unwrap();
    let reloaded = again.session().summary().entries;
    let mine = Target::Latest {
        any_workspace: false,
    };
    let latest = daemon.attach_to(&mine, || "/w".into());
    let missing = daemon.attach_to(&mine, || "/x".into());
    // The reloaded session's recovery pass may relist it in the catalog.
    settle(&daemon);
    std::fs::remove_dir_all(&root).unwrap();

    assert!(hibernated);
    // Its extensions, the input and the answer.
    assert_eq!((listed.len(), listed[0].entries), (1, 3));
    assert_eq!(reloaded, 3);
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
        let attachment = daemon.attach_to(&target, || "/w".into()).unwrap();
        target = Target::Id(attachment.session().id);
        attachment.session().submit(text).unwrap();
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
    assert_eq!(counts[0], [3; 3]);
    assert!(
        counts[1][2] > 3 && counts[1] == [counts[1][2]; 3],
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
        })
        .unwrap();
    let calls = [
        ("sleep", json!({ "seconds": 0 })),
        ("run_command", json!({ "command": "true" })),
    ];
    for (seq, (tool, input)) in (1..).zip(calls) {
        let body = Body::ToolCall {
            tool: tool.into(),
            input: Json::encode(&input).unwrap(),
            id: None,
            card: None,
            by: Audience::Model,
        };
        created
            .writer
            .append_raw(&[Entry::new(
                Seq::new(seq).unwrap(),
                Epoch(created.writer.epoch()),
                body,
            )])
            .unwrap();
    }

    let daemon = Daemon::load(&home, &echo()).unwrap();
    settle(&daemon);
    // Its actor clears the marker after the work it settles.
    drop(daemon);
    let got = bodies(&store, id);
    let still_busy = marked(&home, id);
    std::fs::remove_dir_all(&root).unwrap();

    // The safe sleep runs again; the shell command is not rerun.
    let result = |call| {
        got.iter().find_map(|b| match b {
            Body::ToolResult {
                call: c, outcome, ..
            } if c.get() == call => Some(outcome),
            _ => None,
        })
    };
    assert!(
        matches!(result(1), Some(Outcome::Done { output }) if output.parse::<serde_json::Value>().unwrap()["slept"] == 0),
        "{got:#?}"
    );
    assert_eq!(result(2), Some(&Outcome::Interrupted { output: None }));
    assert_eq!(got.len(), 4);
    assert!(!still_busy);
}

/// A busy session whose log is damaged before its tail is listed as kept
/// as it is, shown, and never resumed or written: its open call does not
/// run again, and input is refused.
#[test]
fn a_damaged_log_is_shown_and_never_written() {
    let (root, home) = home("damaged");
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let id = SessionId::new();
    let mut created = store
        .create(&Header {
            id,
            workspace: "/w".into(),
            parent: None,
        })
        .unwrap();
    let call = Body::ToolCall {
        tool: "sleep".into(),
        input: Json::encode(&json!({ "seconds": 0 })).unwrap(),
        id: None,
        card: None,
        by: Audience::Model,
    };
    let text = |seq| Body::Assistant {
        text: format!("reply {seq}"),
    };
    for (seq, body) in (1..).zip([text(1), call, text(3)]) {
        let entry = Entry::new(Seq::new(seq).unwrap(), Epoch(created.writer.epoch()), body);
        created.writer.append_raw(&[entry]).unwrap();
    }
    let path = home.sessions_dir().join(format!("{id}.jsonl"));
    let mut file = std::fs::read(&path).unwrap();
    // In the call's line, before the last.
    let call = memchr::memchr_iter(b'\n', &file).nth(1).unwrap() + 10;
    file[call] ^= 1;
    std::fs::write(&path, &file).unwrap();

    let daemon = Daemon::load(&home, &echo()).unwrap();
    settle(&daemon);
    let listed = daemon.list();
    let attachment = daemon.attach_to(&Target::Id(id), || "/w".into()).unwrap();
    let refused = attachment.session().submit("hi");
    settle(&daemon);
    drop((attachment, daemon));
    let after = std::fs::read(&path).unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    let why = protocol::ReadOnly::Damaged { seq: 2 };
    assert_eq!(listed[0].read_only, Some(why));
    assert_eq!(listed[0].entries, 1);
    assert_eq!(refused, Err(why.to_string()));
    assert_eq!(after, file, "the log is never touched");
}

/// A session waiting for approval is listed as such once hibernated, and
/// a restarted daemon loads it at once to ask again.
#[test]
fn a_session_waiting_for_approval_stays_waiting_through_hibernation_and_a_restart() {
    let (root, home) = home("asked");
    let config = Config {
        ext: exts("[ext.policy]\nask = [\"bash\"]"),
        ..echo()
    };
    let daemon = Daemon::load(&home, &config).unwrap();
    let attachment = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    let id = attachment.session().id;
    attachment.session().submit("$ echo hi").unwrap();
    settle(&daemon);
    drop(attachment);
    daemon.hibernate(Duration::ZERO);
    let hibernated = daemon.list().remove(0);
    drop(daemon);
    let marked = marked(&home, id);

    let restarted = Daemon::load(&home, &config).unwrap();
    let listed = restarted.list().remove(0);
    let asked = restarted.slots()[0].live().map(|s| s.summary().activity);
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
        ext: exts("[ext.policy]\nask = [\"bash\"]"),
        ..echo()
    };
    let daemon = Daemon::load(&home, &config).unwrap();
    (root, home, daemon)
}

#[test]
fn no_writer_outlives_its_daemon() {
    let (root, home, daemon) = asking("outlives");
    let waiting = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    waiting.session().submit("$ echo hi").unwrap();
    let idle = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    settle(&daemon);
    let ended = [waiting.session().ended(), idle.session().ended()];
    let id = waiting.session().id;
    drop(daemon);

    let over = ended.iter().map(ext::Flag::is_set).collect::<Vec<_>>();
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let at_exit = store.count(id).unwrap();
    // A client still holding on cannot write either: the log is closed.
    let refused = waiting.session().submit("more");
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
    let attachment = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    let id = attachment.session().id;
    attachment.session().submit("$ echo hi").unwrap();
    settle(&daemon);
    let first = attachment.session().ended();
    drop(attachment);

    // Hibernated while its actor still waits for the answer, then wanted
    // at once.
    daemon.hibernate(Duration::ZERO);
    let again = daemon.attach_to(&Target::Id(id), || "/w".into()).unwrap();
    let over = first.is_set();
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
    let parent = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    let id = parent.session().id;
    parent.session().submit("spawn hi").unwrap();
    settle(&daemon);
    drop(parent);
    let store = &*daemon.shared.store;
    let got = bodies(store, id);
    let Body::Link { child, .. } = got[3] else {
        panic!("{got:?}")
    };
    let child_header = store.headers().unwrap().into_iter().find(|h| h.id == child);
    let child_log = bodies(store, child);
    let latest = Target::Latest {
        any_workspace: false,
    };
    let resumed = daemon
        .attach_to(&latest, || "/w".into())
        .unwrap()
        .session()
        .id;
    settle(&daemon);
    // The child working again reports nothing: the parent's watch fired,
    // so the hibernated parent stays unloaded.
    daemon.hibernate(Duration::ZERO);
    let again = daemon
        .attach_to(&Target::Id(child), || "/w".into())
        .unwrap();
    again.session().submit("more").unwrap();
    settle(&daemon);
    drop(again);
    let parent_slot = daemon.slots().into_iter().find(|s| s.header.id == id);
    let parent_loaded = parent_slot.unwrap().live().is_some();
    drop(daemon);

    // A restart finds nothing left to fire.
    let again = Daemon::load(&home, &echo()).unwrap();
    settle(&again);
    let after = bodies(&*again.shared.store, id);
    std::fs::remove_dir_all(&root).unwrap();

    // The `task` call waits on the child it linked, and answers with what
    // the child settled with.
    assert_eq!(
        got[3..],
        [
            Body::Link {
                child,
                input: "hi".into()
            },
            Body::Wait {
                call: CallId::new(3).unwrap(),
                on: protocol::WaitOn::Child {
                    link: Seq::new(4).unwrap()
                }
            },
            Body::Watch { child },
            Body::Fired {
                watch: Seq::new(6).unwrap(),
                outcome: Outcome::Done {
                    output: Json::encode("hi").unwrap()
                }
            },
            Body::ToolResult {
                call: CallId::new(3).unwrap(),
                outcome: Outcome::Done {
                    output: Json::encode("The child session finished:\nhi").unwrap()
                },
                card: None
            },
        ]
    );
    assert!(matches!(&got[2], Body::ToolCall { tool, .. } if tool == "task"));
    assert!(!parent_loaded, "a moot report loaded the parent");
    assert_eq!(child_header.unwrap().parent.unwrap().call, 4);
    assert_eq!(resumed, id, "resuming picks the parent, not its child");
    assert_eq!(
        child_log[1..],
        [
            Body::Input {
                text: "hi".into(),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: None,
            },
            Body::Assistant { text: "hi".into() },
        ]
    );
    assert_eq!(after, got);
}

#[test]
fn a_child_runs_in_its_parents_environment() {
    let (root, home) = home("child-env");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let parent = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    let id = parent.session().id;
    let vars = [("PATH", "/venv/bin"), ("API_TOKEN", "s3cret")];
    let env = Environment {
        cwd: "/w/sub".into(),
        vars: vars.map(|(k, v)| (k.into(), v.into())).into(),
        ..Environment::default()
    };
    parent.session().set_env(env.clone());
    parent.session().submit("spawn hi").unwrap();
    settle(&daemon);
    drop(parent);
    let store = &*daemon.shared.store;
    let child = bodies(store, id).into_iter().find_map(|b| match b {
        Body::Link { child, .. } => Some(child),
        _ => None,
    });
    let opening = bodies(store, child.unwrap()).into_iter().nth(1);
    drop(daemon);
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(opening, Some(Body::Environment(env.redacted())));
}

/// Logs as a daemon killed at various points leaves them: each parent's
/// watch fires once the next daemon has recovered, and never again.
#[test]
#[allow(clippy::too_many_lines)]
fn watches_rearm_after_a_crash_and_fire_once() {
    let (root, home) = home("rearm");
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let write = |header: &Header, bodies: Vec<Body>, busy: bool| {
        let mut created = store.create(header).unwrap();
        for (seq, body) in (1..).zip(bodies) {
            let entry = Entry::new(Seq::new(seq).unwrap(), Epoch(created.writer.epoch()), body);
            created
                .writer
                .append_raw(std::slice::from_ref(&entry))
                .unwrap();
        }
        assert_eq!(
            busy,
            Ledger::of(&store.entries(header.id).unwrap()).activity() != Activity::Idle
        );
    };
    let session = |parent: Option<SessionId>| Header {
        id: SessionId::new(),
        workspace: "/w".into(),
        parent: parent.map(|session| crate::store::Parent { session, call: 3 }),
    };
    // The `task` call, its link, its wait on the child and the watch commit
    // in one batch.
    let spawned = |child: SessionId| {
        vec![
            Body::Input {
                text: "spawn sleep 0".into(),
                mode: protocol::Mode::Queue,
                taken: true,
                origin: None,
            },
            Body::ToolCall {
                tool: "task".into(),
                input: Json::encode(&json!({ "prompt": "sleep 0" })).unwrap(),
                id: None,
                card: None,
                by: Audience::Model,
            },
            Body::Link {
                child,
                input: "sleep 0".into(),
            },
            Body::Wait {
                call: CallId::new(2).unwrap(),
                on: protocol::WaitOn::Child {
                    link: Seq::new(3).unwrap(),
                },
            },
            Body::Watch { child },
        ]
    };
    let opened = || Body::Input {
        text: "sleep 0".into(),
        mode: protocol::Mode::Queue,
        taken: true,
        origin: None,
    };
    let sleep = || Body::ToolCall {
        tool: "sleep".into(),
        input: Json::encode(&json!({ "seconds": 0 })).unwrap(),
        id: None,
        card: None,
        by: Audience::Model,
    };
    let slept = Outcome::Done {
        output: Json::encode(&json!({ "slept": 0 })).unwrap(),
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
        call: CallId::new(2).unwrap(),
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
            matches!(
                log.last(),
                Some(Body::ToolResult { call, .. }) if call.get() == 2
            ),
            "{log:#?}"
        );
    }
}

/// Waits until `done` holds for session `id`'s committed log.
fn until(daemon: &Daemon, id: SessionId, done: impl Fn(&[Body]) -> bool + Sync) -> Vec<Body> {
    let log = std::sync::Mutex::new(Vec::new());
    daemon.wait("the log as expected", PATIENCE, || {
        let mut log = crate::lock(&log);
        *log = bodies(&*daemon.shared.store, id);
        done(&log)
    });
    log.into_inner().unwrap()
}

#[test]
fn an_owner_idles_with_its_children_and_an_interrupt_stops_them_first() {
    let (root, home) = home("owned");
    std::fs::create_dir_all(&root).unwrap();
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let parent = daemon.attach_to(&Target::New, || root.clone()).unwrap();
    let id = parent.session().id;
    parent.session().submit("spawn $ sleep 100").unwrap();
    drop(parent);
    let slot = daemon.slots().into_iter().find(|s| s.header.id == id);
    let session = slot.unwrap().live().unwrap();
    let log = until(&daemon, id, |log| {
        log.iter().any(|b| matches!(b, Body::Watch { .. }))
    });
    let Some(Body::Link { child, .. }) = log.get(3) else {
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
    let aborted = |b: &Body| matches!(b, Body::ToolResult { outcome: Outcome::Done { output }, .. } if output.parse::<serde_json::Value>().unwrap()["aborted"] == true);
    assert!(child_log.iter().any(aborted), "{child_log:#?}");
    // The child settles first, then its parent's call, then the turn.
    let interrupted = Outcome::Interrupted { output: None };
    let tail: Vec<_> = parent_log.iter().rev().take(3).collect();
    assert!(
        matches!(tail[..], [
            Body::Interrupt { .. },
            Body::ToolResult { outcome, .. },
            Body::Fired { .. },
        ] if *outcome == interrupted),
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
        parent: Some(crate::store::Parent {
            session: SessionId::new(),
            call: 2,
        }),
    };
    let mut created = store.create(&header).unwrap();
    let bodies_written = [
        Body::Input {
            text: "sleep 0".into(),
            mode: protocol::Mode::Queue,
            taken: true,
            origin: None,
        },
        Body::ToolCall {
            tool: "sleep".into(),
            input: Json::encode(&json!({ "seconds": 0 })).unwrap(),
            id: None,
            card: None,
            by: Audience::Model,
        },
    ];
    for (seq, body) in (1..).zip(bodies_written) {
        let entry = Entry::new(Seq::new(seq).unwrap(), Epoch(created.writer.epoch()), body);
        created
            .writer
            .append_raw(std::slice::from_ref(&entry))
            .unwrap();
    }
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
                Body::ToolResult {
                    call,
                    outcome: Outcome::Interrupted { output: None },
                    ..
                },
                Body::Interrupt { origin: None },
            ] if call.get() == 2
        ),
        "{got:#?}"
    );
}

/// Keeps every event the daemon emits in tests, and says when one comes.
pub struct Recorder(std::sync::Mutex<Vec<rt::Event>>, rt::Notify);

impl Recorder {
    pub const fn new() -> Self {
        Self(std::sync::Mutex::new(Vec::new()), rt::Notify::new())
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
        self.1.notify();
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
    daemon.wait("the first look", PATIENCE, || EVENTS.count("wake") > 0);

    let attachment = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    let id = attachment.session().id;
    attachment.session().submit("$ true").unwrap();
    settle(&daemon);
    let ended = attachment.session().ended();
    let detached = Instant::now();
    drop(attachment);
    let hibernated = || daemon.slots()[0].live().is_none();
    daemon.wait("hibernation", PATIENCE, hibernated);
    let after = detached.elapsed();
    // The actor's end, which it tells the supervisor, is the last news.
    rt::block_on(ended.wait());
    while anything_within(&daemon, Duration::from_millis(100)) {}
    let wakes = EVENTS.count("wake");

    // Nothing is due for an hour: nothing wakes the supervisor.
    assert!(!anything_within(&daemon, Duration::from_millis(500)));
    let idle_wakes = EVENTS.count("wake") - wakes;
    let names: Vec<_> = (EVENTS.of(id).iter().map(|e| e.name))
        .filter(|&name| name != "commit")
        .collect();
    std::fs::remove_dir_all(&root).unwrap();

    assert!(
        after >= Duration::from_millis(300),
        "hibernated after {after:?}"
    );
    assert_eq!(idle_wakes, 0);
    assert!(wakes < 20, "{wakes} wakes for one turn");
    assert_eq!(names, ["load", "busy", "effect", "settled", "hibernate"]);
}

#[test]
fn a_daemon_after_a_crash_loop_refuses_to_serve_until_reset() {
    use super::{Claim, claim};

    let (root, home) = home("breaker");
    std::fs::create_dir_all(home.run_dir()).unwrap();
    std::fs::write(home.run_dir().join("crashes"), "3\n").unwrap();
    let Claim::Tripped(pid, why) = claim(&home, "b1", None, false, 3).unwrap() else {
        panic!("served after a crash loop");
    };

    pid.set_len(0).unwrap();
    drop(pid);
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
    use protocol::{DaemonMsg, SessionSummary};

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

    let watcher = Outbox::new();
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

    let attachment = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    daemon.wait("the new session listed", PATIENCE, || {
        latest(&|list| list.len() == 1 && list[0].clients == 1)
    });
    drop(attachment);
    daemon.wait("its client gone", PATIENCE, || {
        latest(&|list| list.len() == 1 && list[0].clients == 0)
    });
    // Only changes are sent.
    let sent = seen.lock().unwrap().len();
    while anything_within(&daemon, Duration::from_millis(100)) {}
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
                .attach_to(&Target::New, || "/w".into())
                .unwrap()
                .session()
                .id
        })
        .collect();
    let found = |prefix: String| {
        daemon
            .attach_to(&Target::Prefix(prefix), || "/w".into())
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

/// #1: a session is hibernated and loaded again over and over while
/// input arrives: its seqs run on unbroken, and each load writes under a
/// newer lease than the last, never two at once.
#[test]
fn one_writer_per_session() {
    let (root, home) = home("one-writer");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let mut target = Target::New;
    for n in 0..20 {
        let attachment = daemon.attach_to(&target, || "/w".into()).unwrap();
        target = Target::Id(attachment.session().id);
        attachment.session().submit(&format!("turn {n}")).unwrap();
        drop(attachment);
        // Unloaded as soon as it is idle, maybe while it still answers.
        daemon.hibernate(Duration::ZERO);
    }
    settle(&daemon);
    let Target::Id(id) = target else {
        unreachable!()
    };
    let entries = daemon.shared.store.entries(id).unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    let seqs: Vec<u64> = entries.iter().map(|e| e.seq.get()).collect();
    // Its extensions first, then each input and its answer.
    assert_eq!(seqs, (1..=41).collect::<Vec<_>>());
    assert!(
        entries.windows(2).all(|w| w[0].epoch <= w[1].epoch),
        "{entries:#?}"
    );
    let inputs = entries
        .iter()
        .filter(|e| matches!(e.body, Body::Input { .. }));
    assert_eq!(inputs.count(), 20);
}

/// #16: once hibernated, nothing of a loaded session is held: its frames,
/// transcript and actor go with it.
#[test]
fn hibernating_frees_the_session() {
    let (root, home) = home("frees");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let attachment = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    attachment.session().submit("hi").unwrap();
    settle(&daemon);
    let live = std::sync::Arc::downgrade(attachment.session());
    let ended = attachment.session().ended();
    drop(attachment);
    daemon.hibernate(Duration::ZERO);
    rt::block_on(ended.wait());
    // What lists it lets the stopped session go.
    let listed = daemon.list().remove(0);
    let freed = live.upgrade().is_none();
    drop(daemon);
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(listed.residency, Residency::Hibernated);
    assert!(freed, "a hibernated session is still held");
}

/// A daemon starts, and lists, from the catalog alone: it opens no log,
/// so its start does not grow with the sessions it keeps. Here no log is
/// readable, so a start that opened them would list none.
#[test]
fn cold_start_does_not_grow_with_session_count() {
    const SESSIONS: usize = 5000;
    let (root, home) = home("cold");
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let headers: Vec<Header> = (0..SESSIONS)
        .map(|_| Header {
            id: SessionId::new(),
            workspace: "/w".into(),
            parent: None,
        })
        .collect();
    for h in &headers {
        std::fs::write(home.sessions_dir().join(format!("{}.jsonl", h.id)), "torn").unwrap();
    }
    let catalog = Catalog::open(home.catalog_file(), &store).unwrap();
    assert!(catalog.records().is_empty(), "the logs are unreadable");
    for h in &headers {
        catalog.record(crate::catalog::Record {
            entries: 7,
            ..crate::catalog::Record::new(h.clone())
        });
    }
    catalog.clean().unwrap();
    drop(catalog);

    let began = Instant::now();
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let started = began.elapsed();
    let listed = daemon.list();
    drop(daemon);
    std::fs::remove_dir_all(&root).unwrap();

    assert_eq!(listed.len(), SESSIONS);
    assert!(listed.iter().all(|s| s.entries == 7));
    let ids: Vec<SessionId> = listed.iter().map(|s| s.id).collect();
    let mut made: Vec<SessionId> = headers.iter().map(|h| h.id).collect();
    made.sort_unstable_by_key(|id| std::cmp::Reverse(id.0));
    assert_eq!(ids, made, "newest first");
    assert!(started < Duration::from_secs(2), "started in {started:?}");
}

/// The catalog is derived: lost, the next daemon rebuilds it from the
/// logs, listing every session as it was, and loads at once the one that
/// was left waiting on the user.
#[test]
fn a_lost_catalog_is_rebuilt_from_the_logs() {
    let (root, home, daemon) = asking("rebuilt");
    let mut asked = None;
    for text in ["hi", "$ echo hi", "there"] {
        let attachment = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
        attachment.session().submit(text).unwrap();
        settle(&daemon);
        if text.starts_with('$') {
            asked = Some(attachment.session().id);
        }
    }
    daemon.hibernate(Duration::ZERO);
    let listed = daemon.list();
    drop(daemon);
    std::fs::remove_file(home.catalog_file()).unwrap();

    let config = Config {
        ext: exts("[ext.policy]\nask = [\"bash\"]"),
        ..echo()
    };
    let restarted = Daemon::load(&home, &config).unwrap();
    let relisted = restarted.list();
    let loaded: Vec<SessionId> = restarted.loaded().iter().map(|s| s.id).collect();
    drop(restarted);
    let rebuilt = home.catalog_file().exists();
    std::fs::remove_dir_all(&root).unwrap();

    let shown = |list: &[protocol::SessionSummary]| -> Vec<_> {
        list.iter().map(|s| (s.id, s.entries, s.activity)).collect()
    };
    assert_eq!(shown(&relisted), shown(&listed));
    assert_eq!(loaded, Vec::from_iter(asked));
    assert!(rebuilt);
}

/// The daemon sends the list in the one order every client shows: newest
/// first. No client sorts.
#[test]
fn sessions_are_listed_newest_first() {
    let (_, home) = home("newest");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let mut made = Vec::new();
    for _ in 0..3 {
        let attachment = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
        made.push(attachment.session().id);
    }
    let listed: Vec<_> = daemon.list().iter().map(|s| s.id).collect();
    made.reverse();
    assert_eq!(listed, made);
}

#[test]
fn an_unclean_stop_rebuilds_the_catalog_and_recovers_the_call() {
    let (root, home) = home("unclean");
    let store = Jsonl::open(home.sessions_dir()).unwrap();
    let id = SessionId::new();
    let mut created = store
        .create(&Header {
            id,
            workspace: "/w".into(),
            parent: None,
        })
        .unwrap();
    let catalog = Catalog::open(home.catalog_file(), &store).unwrap();
    catalog.record(crate::catalog::Record::new(Header {
        id,
        workspace: "/w".into(),
        parent: None,
    }));
    drop(catalog);
    let calls = [
        ("sleep", json!({ "seconds": 0 })),
        ("bash", json!({ "command": "true" })),
    ];
    for (seq, (tool, input)) in (1..).zip(calls) {
        let body = Body::ToolCall {
            tool: tool.into(),
            input: Json::encode(&input).unwrap(),
            id: None,
            card: None,
            by: Audience::Model,
        };
        created
            .writer
            .append_raw(&[Entry::new(
                Seq::new(seq).unwrap(),
                Epoch(created.writer.epoch()),
                body,
            )])
            .unwrap();
    }

    let daemon = Daemon::load(&home, &echo()).unwrap();
    settle(&daemon);
    // Its actor clears the marker after the work it settles.
    drop(daemon);
    let got = bodies(&store, id);
    let still_busy = marked(&home, id);
    std::fs::remove_dir_all(&root).unwrap();

    // The safe sleep runs again; the shell command is not rerun.
    let result = |call| {
        got.iter().find_map(|b| match b {
            Body::ToolResult {
                call: c, outcome, ..
            } if c.get() == call => Some(outcome),
            _ => None,
        })
    };
    assert!(
        matches!(result(1), Some(Outcome::Done { output }) if output.parse::<serde_json::Value>().unwrap()["slept"] == 0),
        "{got:#?}"
    );
    assert_eq!(result(2), Some(&Outcome::Interrupted { output: None }));
    assert_eq!(got.len(), 4);
    assert!(!still_busy);
}

#[test]
fn a_shutdown_deadline_leaves_the_catalog_dirty_while_a_load_can_write() {
    let (root, home) = home("shutdown-loading-deadline");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let attached = daemon.attach_to(&Target::New, || "/w".into()).unwrap();
    let slot = daemon.slot(attached.session().id).unwrap();
    slot.unload();
    if let Some(ended) = slot.ending() {
        rt::block_on(ended.wait());
    }
    slot.ending();
    assert!(slot.loading());
    rt::block_on(daemon.shutdown_until(Box::pin(std::future::ready(()))));
    let dirty = home.catalog_file().with_extension("dirty").exists();
    let loaded = Live::start(
        &slot.header,
        &daemon.shared,
        super::Open::Load,
        kernel::Owner::None,
    )
    .unwrap();
    let admitted = daemon.started(&slot, Ok(std::sync::Arc::clone(&loaded)));
    loaded.stop();
    rt::block_on(loaded.ended.wait());
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
    assert!(dirty, "a load can still produce a writer");
    assert!(admitted.is_err(), "shutdown must close admission");
}

#[test]
fn shutdown_waits_for_reserved_loads_and_stops_their_actors_before_cleaning() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let (root, home) = home("shutdown-reserved-load");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let slot = daemon
        .new_slot(Header {
            id: SessionId::new(),
            workspace: "/w".into(),
            parent: None,
        })
        .unwrap();
    rt::block_on(async {
        let mut shutdown = Box::pin(daemon.shutdown());
        poll_fn(|cx| {
            assert!(shutdown.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(home.catalog_file().with_extension("dirty").exists());
        assert!(daemon.create("/new".into()).await.is_err());
        assert!(daemon.live(&slot).await.is_err());
        assert!(daemon.start(&slot, super::Open::Create).await.is_err());
        shutdown.await;
    });
    assert!(slot.ending().is_none());
    assert!(!home.catalog_file().with_extension("dirty").exists());
    drop(daemon);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_fork_never_changes_its_source() {
    let (root, home) = home("fork-source");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let source = daemon.attach_to(&Target::New, std::env::temp_dir).unwrap();
    source.session().submit("first").unwrap();
    settle(&daemon);
    let id = source.session().id;
    let path = home.sessions_dir().join(format!("{id}.jsonl"));
    let before = std::fs::read(&path).unwrap();
    let copied = daemon
        .attach_to(
            &Target::Fork {
                session: id.to_string(),
                seq: None,
            },
            PathBuf::new,
        )
        .unwrap();
    assert_ne!(copied.session().id, id);
    copied.session().submit("only in the fork").unwrap();
    settle(&daemon);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(copied.session().workspace, source.session().workspace);
    let source_entries = daemon.shared.store.entries(id).unwrap();
    let copied_entries = daemon.shared.store.entries(copied.session().id).unwrap();
    assert_eq!(copied_entries[..source_entries.len()], source_entries);
    assert!(
        matches!(&copied_entries[source_entries.len()].body, Body::Ext { ext, state, data }
        if ext == "fork" && state == "parent" && data.parse::<protocol::ForkParent>().unwrap().session == id)
    );
    drop((copied, source, daemon));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn every_forkable_prefix_has_the_sources_view_and_unfinished_prefixes_create_nothing() {
    let (root, home) = home("fork-prefixes");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let source = daemon.attach_to(&Target::New, std::env::temp_dir).unwrap();
    source.session().submit("first").unwrap();
    settle(&daemon);
    source.session().submit("second").unwrap();
    settle(&daemon);
    let id = source.session().id;
    let entries = daemon.shared.store.entries(id).unwrap();
    let mut accepted = 0;
    let mut rejected = 0;
    for seq in 1..=entries.len() {
        let count = daemon.shared.store.headers().unwrap().len();
        let result = daemon.attach_to(
            &Target::Fork {
                session: id.to_string(),
                seq: Seq::new(seq as u64),
            },
            PathBuf::new,
        );
        match result {
            Ok(fork) => {
                accepted += 1;
                settle(&daemon);
                let prefix = &entries[..seq];
                let copied = daemon.shared.store.entries(fork.session().id).unwrap();
                assert_eq!(&copied[..seq], prefix);
                let mut source_view = protocol::view::SessionView::default();
                let mut fork_view = protocol::view::SessionView::default();
                for entry in prefix {
                    source_view.fold(&protocol::DaemonMsg::Entry(entry.clone()));
                }
                for entry in &copied[..seq] {
                    fork_view.fold(&protocol::DaemonMsg::Entry(entry.clone()));
                }
                assert_eq!(source_view, fork_view);
            }
            Err(error) => {
                rejected += 1;
                assert!(error.to_string().contains("rest point"), "{error}");
                assert_eq!(daemon.shared.store.headers().unwrap().len(), count);
            }
        }
    }
    assert!(accepted > 0 && rejected > 0);
    drop((source, daemon));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_hibernated_source_stays_hibernated_when_forked() {
    let (root, home) = home("fork-hibernated");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let source = daemon.attach_to(&Target::New, std::env::temp_dir).unwrap();
    source.session().submit("first").unwrap();
    settle(&daemon);
    let id = source.session().id;
    drop(source);
    daemon.hibernate(Duration::ZERO);
    daemon.wait("source hibernated", PATIENCE, || {
        daemon.slot(id).unwrap().live().is_none()
    });
    let before = std::fs::read(home.sessions_dir().join(format!("{id}.jsonl"))).unwrap();
    let fork = daemon
        .attach_to(
            &Target::Fork {
                session: id.to_string(),
                seq: None,
            },
            PathBuf::new,
        )
        .unwrap();
    assert!(daemon.slot(id).unwrap().live().is_none());
    fork.session().submit("new branch").unwrap();
    settle(&daemon);
    assert_eq!(
        std::fs::read(home.sessions_dir().join(format!("{id}.jsonl"))).unwrap(),
        before
    );
    drop((fork, daemon));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_pending_approval_cannot_be_forked_or_answered_in_a_second_session() {
    let (root, _, daemon) = asking("fork-approval");
    let source = daemon.attach_to(&Target::New, std::env::temp_dir).unwrap();
    source.session().submit("$ echo never").unwrap();
    settle(&daemon);
    assert_eq!(source.session().summary().activity, Activity::Approval);
    let id = source.session().id;
    let before = daemon.shared.store.entries(id).unwrap();
    let result = rt::block_on(daemon.fork(&id.to_string(), None));
    assert!(matches!(result, Err(protocol::ForkError::Unfinished)));
    assert_eq!(daemon.shared.store.headers().unwrap().len(), 1);
    assert_eq!(daemon.shared.store.entries(id).unwrap(), before);
    drop((source, daemon));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_fork_keeps_the_selected_recorded_environment_without_live_secrets() {
    let (root, home) = home("fork-environment");
    let daemon = Daemon::load(&home, &echo()).unwrap();
    let source = daemon.attach_to(&Target::New, std::env::temp_dir).unwrap();
    let old = Environment {
        cwd: root.join("old"),
        vars: std::collections::BTreeMap::from([
            ("EXAMPLE".into(), "old".into()),
            ("PASSWORD".into(), "fixture-secret".into()),
        ]),
        ..Environment::default()
    };
    source.session().set_env(old.clone());
    source.session().submit("first").unwrap();
    settle(&daemon);
    let boundary = source.session().summary().entries;
    source.session().set_env(Environment {
        cwd: root.join("new"),
        ..Environment::default()
    });
    source.session().submit("second").unwrap();
    settle(&daemon);
    let fork = daemon
        .attach_to(
            &Target::Fork {
                session: source.session().id.to_string(),
                seq: Seq::new(boundary),
            },
            PathBuf::new,
        )
        .unwrap();
    settle(&daemon);
    let log = daemon.shared.store.entries(fork.session().id).unwrap();
    let env = Ledger::of(&log).env().cloned().unwrap();
    assert_eq!(env.cwd, old.cwd);
    assert_eq!(env.vars.get("EXAMPLE").unwrap(), "old");
    assert!(!env.vars.contains_key("PASSWORD"));
    assert_eq!(env.redacted, ["PASSWORD"]);
    drop((fork, source, daemon));
    std::fs::remove_dir_all(root).unwrap();
}
