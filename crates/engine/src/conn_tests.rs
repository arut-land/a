//! Connections as clients make them: the hello, pipelining, numbered
//! mutations and the hello's versions.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::store::{Header, Jsonl, LogStore};
use protocol::view::SessionView;
use protocol::{
    Body, CallId, Caps, ClientDecl, ClientId, ClientMsg, DaemonMsg, Entry, Environment, Epoch,
    PROTO, Rejection, Role, Seq, SessionId, SettingsChange, Target, read_frame, write_frame,
};
use sys::Home;

use super::outbox::Outbox;
use super::server::handle;
use super::{Config, Daemon, Lifecycle, Owner};

struct Fixture {
    root: PathBuf,
    daemon: Owner,
    life: Arc<Lifecycle>,
    config: Config,
}

impl Fixture {
    fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    /// A daemon on a home that `seed` writes sessions into first.
    fn with(name: &str, seed: impl FnOnce(&Home)) -> Self {
        let root = tempfile::Builder::new()
            .prefix(&format!("a-conn-{name}-"))
            .tempdir()
            .unwrap()
            .keep();
        let home = Home::from(root.clone());
        seed(&home);
        let config = Config {
            agent: Some(ext_fixtures::ECHO.into()),
            ..Config::default()
        };
        let daemon = Daemon::load(&home, &config).unwrap();
        let life = Arc::new(Lifecycle::new("test".into(), home.runtime()));
        Self {
            root,
            daemon,
            life,
            config,
        }
    }

    /// As after the daemon exits and the next starts on the same home.
    fn restart(&mut self) {
        let home = Home::from(self.root.clone());
        self.daemon = Daemon::load(&home, &self.config).unwrap();
    }

    /// A client connection, served as the daemon serves one.
    fn connect(&self) -> Peer<'_> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (daemon, life) = (Arc::clone(&self.daemon), Arc::clone(&self.life));
        let rt = Arc::clone(&daemon.shared.rt);
        let finished = ext::Flag::default();
        let done = finished.clone();
        rt.spawn(Box::pin(async move {
            let _ = handle(
                &daemon,
                &life,
                sys::OwnedConnection::try_from(theirs).unwrap(),
            )
            .await;
            done.set();
        }));
        ours.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Peer {
            reader: BufReader::new(ours.try_clone().unwrap()),
            writer: ours,
            finished,
            _owner: &self.daemon,
        }
    }
}

impl Fixture {
    fn settle(&self) {
        for live in self.daemon.loaded() {
            live.barrier();
        }
        self.daemon.wait("settled", Duration::from_secs(10), || {
            !self.daemon.has_work()
        });
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.settle();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Peer<'a> {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    finished: ext::Flag,
    _owner: &'a Owner,
}

impl Drop for Peer<'_> {
    fn drop(&mut self) {
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
        rt::block_on(self.finished.wait());
    }
}

impl Peer<'_> {
    fn send(&mut self, msg: &ClientMsg) {
        write_frame(&mut self.writer, msg).unwrap();
    }

    fn recv(&mut self) -> DaemonMsg {
        read_frame(&mut self.reader)
            .unwrap()
            .expect("the daemon hung up")
    }

    /// Messages up to and including the first `stop` accepts.
    fn until(&mut self, stop: impl Fn(&DaemonMsg) -> bool) -> Vec<DaemonMsg> {
        let mut seen = Vec::new();
        loop {
            let msg = self.recv();
            let done = stop(&msg);
            seen.push(msg);
            if done {
                return seen;
            }
        }
    }

    /// The next message other than an entry, a delta or a replay's end.
    fn answer(&mut self) -> DaemonMsg {
        loop {
            match self.recv() {
                DaemonMsg::Entry(_)
                | DaemonMsg::Delta { .. }
                | DaemonMsg::Activity { .. }
                | DaemonMsg::Synced { .. } => {}
                msg => return msg,
            }
        }
    }
}

fn hello(caps: Caps) -> ClientMsg {
    ClientMsg::Hello {
        proto: PROTO,
        build: "test".into(),
        min: None,
        caps,
        client: None,
    }
}

/// The hello of client `client`.
fn hello_as(client: ClientId) -> ClientMsg {
    ClientMsg::Hello {
        proto: PROTO,
        build: "test".into(),
        min: None,
        caps: Caps::ALL,
        client: Some(client),
    }
}

/// The inputs session `id`'s log holds.
fn inputs(fixture: &Fixture, id: SessionId) -> usize {
    let entries = fixture.daemon.shared.store.entries(id).unwrap();
    entries.iter().filter(|e| is_input(&e.body)).count()
}

/// #12: client `client` sends input 1 to a new session, and again on a
/// connection after its first, whichever `again` makes; it is acked both
/// times, and done once.
fn resent_once(fixture: &mut Fixture, again: impl FnOnce(&mut Fixture)) {
    let client = ClientId::new();
    let mut peer = fixture.connect();
    peer.send(&hello_as(client));
    peer.send(&attach(Target::New, 0, ClientDecl::default()));
    peer.send(&input("hi", 1));
    let acked = |m: &DaemonMsg| matches!(m, DaemonMsg::Ack { client_seq: 1 });
    let seen = peer.until(acked);
    let attached = seen.iter().find_map(|m| match m {
        DaemonMsg::Attached { session, .. } => Some(*session),
        _ => None,
    });
    let session = attached.unwrap();
    fixture.settle();
    drop(peer);
    again(fixture);
    let mut peer = fixture.connect();
    peer.send(&hello_as(client));
    peer.send(&attach(Target::Id(session), 0, ClientDecl::default()));
    peer.send(&input("hi", 1));
    peer.until(acked);
    fixture.settle();
    assert_eq!(inputs(fixture, session), 1);
}

#[test]
fn a_resend_after_reconnect_runs_once() {
    resent_once(&mut Fixture::new("resend"), |_| {});
}

#[test]
fn a_resend_after_restart_runs_once() {
    resent_once(&mut Fixture::new("resend-restart"), Fixture::restart);
}

fn attach(target: Target, since: u64, decl: ClientDecl) -> ClientMsg {
    ClientMsg::Attach {
        target,
        workspace: "/w".into(),
        since,
        env: None,
        decl,
    }
}

fn input(text: &str, seq: u64) -> ClientMsg {
    ClientMsg::Input {
        text: text.into(),
        client_seq: Some(seq),
        mode: protocol::Mode::Queue,
    }
}

fn origin(_client: u32, client_seq: u64) -> u64 {
    client_seq
}

/// Whether `body` is a client's input.
fn is_input(body: &Body) -> bool {
    matches!(body, Body::Input { .. })
}

fn users(msgs: &[DaemonMsg]) -> usize {
    msgs.iter()
        .filter(|m| matches!(m, DaemonMsg::Entry(Entry { body, .. }) if is_input(body)))
        .count()
}

#[test]
fn a_pipelined_hello_attach_and_input_are_answered_in_order() {
    let fixture = Fixture::new("pipelined");
    let mut peer = fixture.connect();
    peer.send(&hello(Caps::ALL));
    peer.send(&attach(Target::New, 0, ClientDecl::default()));
    peer.send(&input("hi", 1));
    let DaemonMsg::Welcome { caps, .. } = peer.recv() else {
        panic!("no welcome");
    };
    assert_eq!(caps, Caps::SERVED);
    assert!(matches!(peer.recv(), DaemonMsg::Attached { .. }));
    // A new session's first entry records its extensions.
    let first = peer.recv();
    assert!(
        matches!(&first, DaemonMsg::Entry(e) if matches!(e.body, Body::Extensions { .. })),
        "{first:?}"
    );
    assert!(matches!(
        peer.recv(),
        DaemonMsg::Activity {
            seq: 1,
            activity: protocol::Activity::Idle
        }
    ));
    assert!(matches!(peer.recv(), DaemonMsg::Synced { seq: 1 }));
    // The input is logged before it is acknowledged, with what its step
    // logged after it.
    let logged = peer.recv();
    assert!(
        matches!(&logged, DaemonMsg::Entry(e) if is_input(&e.body)),
        "{logged:?}"
    );
    let seen = peer.until(|m| matches!(m, DaemonMsg::Ack { client_seq: o } if *o == origin(1, 1)));
    assert_eq!(users(&seen), 0, "the input was already seen");
    assert!(
        seen.iter()
            .any(|m| matches!(m, DaemonMsg::Entry(e) if matches!(e.body, Body::Assistant { .. }))),
        "{seen:?}"
    );
}

/// Mutations sent one behind another go on to the session as they are
/// read, without waiting for the one before to commit, and are answered
/// in the order sent; what is not a mutation waits for their answers.
#[test]
fn pipelined_mutations_are_answered_in_order() {
    let fixture = Fixture::new("pipelined-mutations");
    let mut peer = fixture.connect();
    peer.send(&hello(Caps::ALL));
    peer.send(&attach(Target::New, 0, ClientDecl::default()));
    for seq in 1..=3 {
        peer.send(&input(&format!("m{seq}"), seq * 2 - 1));
        peer.send(&ClientMsg::Interrupt {
            client_seq: Some(seq * 2),
        });
    }
    peer.send(&ClientMsg::List);
    assert!(matches!(peer.answer(), DaemonMsg::Welcome { .. }));
    assert!(matches!(peer.answer(), DaemonMsg::Attached { .. }));
    let answers: Vec<u64> = (0..6)
        .map(|_| match peer.answer() {
            DaemonMsg::Ack { client_seq } => client_seq,
            msg => panic!("{msg:?}"),
        })
        .collect();
    assert_eq!(answers, [1, 2, 3, 4, 5, 6]);
    assert!(matches!(peer.answer(), DaemonMsg::Sessions { .. }));
}

/// One order for every client's picker and `a ls`: no client sorts.
#[test]
fn sessions_are_listed_newest_first() {
    let fixture = Fixture::new("newest-first");
    let mut ids = Vec::new();
    for _ in 0..3 {
        let mut peer = fixture.connect();
        peer.send(&hello(Caps::ALL));
        peer.send(&attach(Target::New, 0, ClientDecl::default()));
        assert!(matches!(peer.answer(), DaemonMsg::Welcome { .. }));
        let DaemonMsg::Attached { session, .. } = peer.answer() else {
            panic!("not attached");
        };
        ids.push(session);
        peer.send(&ClientMsg::Detach);
    }
    let mut peer = fixture.connect();
    peer.send(&hello(Caps::ALL));
    peer.send(&ClientMsg::List);
    assert!(matches!(peer.answer(), DaemonMsg::Welcome { .. }));
    let DaemonMsg::Sessions { sessions } = peer.answer() else {
        panic!("no list");
    };
    ids.reverse();
    assert_eq!(sessions.iter().map(|s| s.id).collect::<Vec<_>>(), ids);
}

#[test]
fn a_repeated_mutation_is_answered_again_but_not_done_again() {
    let fixture = Fixture::new("repeat");
    let mut peer = fixture.connect();
    peer.send(&hello(Caps::ALL));
    peer.send(&attach(Target::New, 0, ClientDecl::default()));
    peer.send(&input("hi", 1));
    peer.send(&input("hi", 1));
    peer.send(&ClientMsg::Approve {
        call: CallId::new(9).unwrap(),
        approved: true,
        client_seq: Some(2),
    });
    peer.send(&ClientMsg::Approve {
        call: CallId::new(9).unwrap(),
        approved: true,
        client_seq: Some(2),
    });
    assert!(matches!(peer.answer(), DaemonMsg::Welcome { .. }));
    assert!(matches!(peer.answer(), DaemonMsg::Attached { .. }));
    let acked = |m: DaemonMsg| matches!(m, DaemonMsg::Ack { client_seq: o } if o == origin(1, 1));
    assert!(acked(peer.answer()));
    assert!(acked(peer.answer()));
    for _ in 0..2 {
        assert!(matches!(
            peer.answer(),
            DaemonMsg::Rejected { client_seq: o, reason: Rejection::NotWaiting, .. } if o == origin(1, 2)
        ));
    }
    // An older one, sent again, is answered as it was.
    peer.send(&input("hi", 1));
    assert!(acked(peer.answer()));
    peer.send(&ClientMsg::List);
    let DaemonMsg::Sessions { sessions } = peer.answer() else {
        panic!("no list");
    };
    fixture.settle();
    let entries = fixture.daemon.shared.store.entries(sessions[0].id).unwrap();
    let inputs = entries.iter().filter(|e| is_input(&e.body)).count();
    assert_eq!(inputs, 1);
}

#[test]
fn a_viewer_cannot_change_the_session() {
    let fixture = Fixture::new("viewer");
    let mut peer = fixture.connect();
    peer.send(&hello(Caps::ALL));
    let viewer = ClientDecl {
        role: Role::Viewer,
        ..ClientDecl::default()
    };
    peer.send(&attach(Target::New, 0, viewer));
    peer.send(&input("hi", 1));
    peer.send(&ClientMsg::Declare(ClientDecl::default()));
    peer.send(&input("hi", 2));
    assert!(matches!(peer.answer(), DaemonMsg::Welcome { .. }));
    assert!(matches!(peer.answer(), DaemonMsg::Attached { .. }));
    assert!(matches!(
        peer.answer(),
        DaemonMsg::Rejected {
            reason: Rejection::ReadOnly,
            ..
        }
    ));
    assert!(matches!(peer.answer(), DaemonMsg::Ack { client_seq: o } if o == origin(1, 2)));
}

#[test]
fn a_mismatched_hello_gets_the_range_and_nothing_behind_it_is_done() {
    let fixture = Fixture::new("mismatch");
    let mut peer = fixture.connect();
    peer.send(&ClientMsg::Hello {
        proto: PROTO + 2,
        build: "test".into(),
        min: Some(PROTO + 1),
        caps: Caps::ALL,
        client: None,
    });
    peer.send(&attach(Target::New, 0, ClientDecl::default()));
    peer.send(&input("hi", 1));
    assert!(matches!(
        peer.recv(),
        DaemonMsg::Mismatch {
            supported: PROTO,
            min: None
        }
    ));
    drop(peer);
    assert_eq!(fixture.daemon.list(), []);

    // A range that reaches back to ours is welcomed at ours.
    let mut peer = fixture.connect();
    peer.send(&ClientMsg::Hello {
        proto: PROTO + 1,
        build: "test".into(),
        min: Some(PROTO),
        caps: Caps::NONE,
        client: None,
    });
    assert!(matches!(
        peer.recv(),
        DaemonMsg::Welcome { proto: PROTO, .. }
    ));
}

/// A session longer than a snapshot is worth: a client far behind gets
/// one, a client near the end the replay.
#[test]
fn far_behind_a_client_gets_a_snapshot() {
    let id = SessionId::new();
    let fixture = Fixture::with("snapshot", |home| {
        let store = Jsonl::open(home.sessions_dir()).unwrap();
        let created = store
            .create(&Header {
                id,
                workspace: "/w".into(),
                parent: None,
            })
            .unwrap();
        let (epoch, mut writer) = (created.writer.epoch(), created.writer);
        let entries: Vec<Entry> = (1..=300)
            .map(|seq| {
                let text = format!("m{seq}");
                let body = Body::Input {
                    text,
                    mode: protocol::Mode::Queue,
                    taken: true,
                    origin: None,
                };
                Entry::new(Seq::new(seq).unwrap(), Epoch(epoch), body)
            })
            .collect();
        writer.append_raw(&entries).unwrap();
    });
    let replay = |since| {
        let mut peer = fixture.connect();
        peer.send(&hello(Caps::ALL));
        peer.send(&attach(Target::Id(id), since, ClientDecl::default()));
        peer.until(|m| matches!(m, DaemonMsg::Synced { .. }))
    };
    let snapshotted = replay(10);
    let Some(DaemonMsg::Snapshot(view)) = snapshotted.get(2) else {
        panic!("no snapshot: {snapshotted:?}");
    };
    // Each was answered as the session recovered, before the attach.
    assert_eq!((view.items.len(), view.seq), (600, 600));
    assert_eq!(users(&snapshotted), 0);
    let replayed = replay(400);
    assert!(
        replayed
            .iter()
            .all(|m| !matches!(m, DaemonMsg::Snapshot(_)))
    );
    let entries = replayed.iter().filter(|m| matches!(m, DaemonMsg::Entry(_)));
    assert_eq!(entries.count(), 200);
}

/// A client that stops reading only lags: it reads what it missed from the
/// session's shared frames once it reads again, and ends up with every
/// entry.
#[test]
fn a_client_that_falls_behind_is_caught_up_from_the_log() {
    let fixture = Fixture::new("behind");
    let attachment = fixture
        .daemon
        .attach_to(&Target::New, || "/w".into())
        .unwrap();
    let session = attachment.session();
    let outbox = Outbox::new();
    rt::block_on(session.attach(outbox.clone(), 0));
    let big = "x".repeat(1 << 20);
    for _ in 0..6 {
        session.submit(&big).unwrap();
    }
    fixture.settle();

    let mut view = SessionView::default();
    while let Some(frame) = outbox.try_next() {
        let msg: DaemonMsg = read_frame(&mut frame.as_bytes()).unwrap().unwrap();
        view.fold(&msg);
    }
    let log = fixture.daemon.shared.store.entries(session.id).unwrap();
    let whole = SessionView::from_log(session.id, "/w".into(), &log);
    assert_eq!(view.state_hash(), whole.state_hash());
}

/// Sends `change` on a fresh connection and returns the answer.
fn change_settings(fixture: &Fixture, change: Option<SettingsChange>) -> DaemonMsg {
    let mut peer = fixture.connect();
    peer.send(&hello(Caps::ALL));
    peer.send(&ClientMsg::Settings { change });
    assert!(matches!(peer.recv(), DaemonMsg::Welcome { .. }));
    peer.recv()
}

#[test]
fn settings_saved_at_once_by_many_clients_all_land() {
    let fixture = Arc::new(Fixture::new("settings"));
    std::fs::write(
        fixture.root.join("config.toml"),
        "# mine\nmodel = \"a:b\"\n[env]\nKEPT = \"1\"\n",
    )
    .unwrap();
    let savers: Vec<_> = (0..4)
        .map(|client| {
            let fixture = Arc::clone(&fixture);
            std::thread::spawn(move || {
                for n in 0..10 {
                    let change = SettingsChange {
                        env: [(format!("K{client}_{n}"), Some(n.to_string()))].into(),
                        ..SettingsChange::default()
                    };
                    let answer = change_settings(&fixture, Some(change));
                    assert!(
                        matches!(answer, DaemonMsg::Settings { error: None, .. }),
                        "{answer:?}"
                    );
                }
            })
        })
        .collect();
    for saver in savers {
        saver.join().unwrap();
    }
    let DaemonMsg::Settings {
        settings,
        error: None,
    } = change_settings(&fixture, None)
    else {
        panic!("no settings");
    };
    assert_eq!(settings.model.as_deref(), Some("a:b"));
    assert_eq!(settings.env.len(), 41, "{:?}", settings.env);
    let text = std::fs::read_to_string(fixture.root.join("config.toml")).unwrap();
    assert!(text.starts_with("# mine\n"), "{text}");

    // A change the file cannot take is refused, and says why.
    let bad = SettingsChange {
        env: [("NOT A NAME".into(), Some("x".into()))].into(),
        ..SettingsChange::default()
    };
    let DaemonMsg::Settings {
        settings: unchanged,
        error: Some(error),
    } = change_settings(&fixture, Some(bad))
    else {
        panic!("not refused");
    };
    assert!(error.contains("not a variable name"), "{error}");
    assert_eq!(unchanged, settings);
}

#[test]
fn the_daemon_finds_the_workspace_from_the_clients_directory_and_git_variables() {
    let fixture = Fixture::new("workspace");
    let repo = std::fs::canonicalize(&fixture.root).unwrap().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src/deep")).unwrap();
    std::fs::create_dir_all(repo.join("other")).unwrap();
    let attached = |env: Option<Environment>| {
        let mut peer = fixture.connect();
        peer.send(&hello(Caps::NONE));
        peer.send(&ClientMsg::Attach {
            target: Target::New,
            workspace: repo.join("src/deep"),
            since: 0,
            env,
            decl: ClientDecl::default(),
        });
        assert!(matches!(peer.recv(), DaemonMsg::Welcome { .. }));
        let DaemonMsg::Attached { workspace, .. } = peer.recv() else {
            panic!("not attached");
        };
        workspace
    };
    assert_eq!(attached(None), repo);
    let work_tree = Environment::new(
        repo.join("src/deep"),
        [("GIT_WORK_TREE".into(), repo.join("other").into_os_string())],
        false,
    );
    assert_eq!(attached(Some(work_tree)), repo.join("other"));
}

#[test]
fn fork_refusals_are_typed_and_leave_the_source_attachment_usable() {
    let fixture = Fixture::new("fork-refusals");
    let mut peer = fixture.connect();
    peer.send(&hello(Caps::ALL));
    assert!(matches!(peer.answer(), DaemonMsg::Welcome { .. }));
    peer.send(&attach(Target::New, 0, ClientDecl::default()));
    let DaemonMsg::Attached { session, .. } = peer.answer() else {
        panic!("not attached")
    };
    peer.send(&input("complete", 1));
    assert!(matches!(peer.answer(), DaemonMsg::Ack { .. }));
    fixture.settle();
    let log = fixture.daemon.shared.store.entries(session).unwrap();
    let pending = log
        .iter()
        .find(|entry| matches!(entry.body, Body::Input { .. }))
        .unwrap()
        .seq;
    for (source, seq, decl, expected) in [
        (
            "no-such-session".to_owned(),
            None,
            ClientDecl::default(),
            protocol::ForkError::SourceNotFound,
        ),
        (
            session.to_string(),
            Seq::new(u64::MAX),
            ClientDecl::default(),
            protocol::ForkError::InvalidSequence,
        ),
        (
            session.to_string(),
            Some(pending),
            ClientDecl::default(),
            protocol::ForkError::Unfinished,
        ),
        (
            session.to_string(),
            None,
            ClientDecl {
                role: Role::Viewer,
                ..ClientDecl::default()
            },
            protocol::ForkError::Viewer,
        ),
    ] {
        peer.send(&attach(
            Target::Fork {
                session: source,
                seq,
            },
            0,
            decl,
        ));
        assert!(matches!(peer.answer(), DaemonMsg::ForkRejected { reason } if reason == expected));
    }
    peer.send(&input("still attached", 2));
    assert!(matches!(peer.answer(), DaemonMsg::Ack { .. }));
    fixture.settle();
    assert_eq!(fixture.daemon.shared.store.headers().unwrap().len(), 1);
    assert!(
        fixture
            .daemon
            .shared
            .store
            .entries(session)
            .unwrap()
            .iter()
            .any(
                |entry| matches!(&entry.body, Body::Input { text, .. } if text == "still attached")
            )
    );
}
