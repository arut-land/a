//! Connections as clients make them: the hello, pipelining, numbered
//! mutations, capabilities, and the old clients a daemon keeps serving.

use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::view::SessionView;
use protocol::{
    Body, Caps, ClientDecl, ClientMsg, DaemonMsg, Entry, Origin, PROTO, Rejection, Role,
    SettingsChange, Target, read_frame, write_frame,
};
use store::{Header, Jsonl, LogStore};
use sys::Home;

use super::outbox::{Next, Outbox};
use super::{Config, Daemon, Lifecycle, handle};

struct Fixture {
    root: PathBuf,
    daemon: Arc<Daemon>,
    life: Arc<Lifecycle>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    /// A daemon on a home that `seed` writes sessions into first.
    fn with(name: &str, seed: impl FnOnce(&Home)) -> Self {
        let root = std::env::temp_dir().join(format!("a-conn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = Home::from(root.clone());
        seed(&home);
        let config = Config {
            agent: Some(agent::Kind::Echo),
            ..Config::default()
        };
        let daemon = Daemon::load(&home, &config).unwrap();
        let life = Arc::new(Lifecycle::new("test".into(), home.runtime()));
        Self { root, daemon, life }
    }

    /// A client connection, served as the daemon serves one.
    fn connect(&self) -> Peer {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (daemon, life) = (Arc::clone(&self.daemon), Arc::clone(&self.life));
        std::thread::spawn(move || handle(&daemon, &life, &theirs));
        ours.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Peer {
            reader: BufReader::new(ours.try_clone().unwrap()),
            writer: ours,
        }
    }
}

impl Fixture {
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.daemon.has_work() {
            assert!(Instant::now() < deadline, "work never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.settle();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Peer {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Peer {
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
                DaemonMsg::Entry(_) | DaemonMsg::Delta { .. } | DaemonMsg::Synced { .. } => {}
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
    }
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
    }
}

fn origin(client: u32, client_seq: u64) -> Origin {
    Origin { client, client_seq }
}

fn users(msgs: &[DaemonMsg]) -> usize {
    msgs.iter()
        .filter(|m| {
            matches!(
                m,
                DaemonMsg::Entry(Entry {
                    body: Body::User { .. },
                    ..
                })
            )
        })
        .count()
}

#[test]
fn a_pipelined_hello_attach_and_input_are_answered_in_order() {
    let fixture = Fixture::new("pipelined");
    let mut peer = fixture.connect();
    peer.send(&hello(Caps::ALL));
    peer.send(&attach(Target::New, 0, ClientDecl::default()));
    peer.send(&input("hi", 1));
    let DaemonMsg::Welcome { caps, client, .. } = peer.recv() else {
        panic!("no welcome");
    };
    assert_eq!((caps, client), (Caps::ALL, 1));
    assert!(matches!(peer.recv(), DaemonMsg::Attached { .. }));
    assert!(matches!(peer.recv(), DaemonMsg::Synced { seq: 0 }));
    // The input is logged before it is acknowledged.
    let logged = peer.recv();
    assert!(
        matches!(&logged, DaemonMsg::Entry(e) if matches!(e.body, Body::User { .. })),
        "{logged:?}"
    );
    assert!(matches!(peer.recv(), DaemonMsg::Ack { origin: o } if o == origin(1, 1)));
    let seen = peer
        .until(|m| matches!(m, DaemonMsg::Entry(e) if matches!(e.body, Body::Assistant { .. })));
    assert_eq!(users(&seen), 0, "the input was already seen");
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
        call: 9,
        approved: true,
        client_seq: Some(2),
    });
    peer.send(&ClientMsg::Approve {
        call: 9,
        approved: true,
        client_seq: Some(2),
    });
    assert!(matches!(peer.answer(), DaemonMsg::Welcome { .. }));
    assert!(matches!(peer.answer(), DaemonMsg::Attached { .. }));
    let acked = |m: DaemonMsg| matches!(m, DaemonMsg::Ack { origin: o } if o == origin(1, 1));
    assert!(acked(peer.answer()));
    assert!(acked(peer.answer()));
    for _ in 0..2 {
        assert!(matches!(
            peer.answer(),
            DaemonMsg::Rejected { origin: o, reason: Rejection::NotWaiting, .. } if o == origin(1, 2)
        ));
    }
    // A seq older than the last, never answered, is stale.
    peer.send(&input("again", 0));
    assert!(matches!(
        peer.answer(),
        DaemonMsg::Rejected {
            reason: Rejection::Stale,
            ..
        }
    ));
    peer.send(&ClientMsg::List);
    let DaemonMsg::Sessions { sessions } = peer.answer() else {
        panic!("no list");
    };
    fixture.settle();
    let entries = fixture
        .daemon
        .shared
        .store
        .load(sessions[0].id)
        .unwrap()
        .entries;
    let inputs = entries
        .iter()
        .filter(|e| matches!(e.body, Body::User { .. }))
        .count();
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
    assert!(matches!(peer.answer(), DaemonMsg::Ack { origin: o } if o == origin(1, 2)));
}

/// What a client of the first protocol 1 builds sends: no capabilities, no
/// numbers, a bare interrupt. It gets only what such a client knows.
#[test]
fn an_old_client_is_served_as_before() {
    let fixture = Fixture::new("old");
    let mut peer = fixture.connect();
    let raw = |peer: &mut Peer, json: &str| {
        let len = u32::try_from(json.len()).unwrap();
        peer.writer.write_all(&len.to_le_bytes()).unwrap();
        peer.writer.write_all(json.as_bytes()).unwrap();
    };
    raw(&mut peer, r#"{"t":"hello","proto":1,"build":"old"}"#);
    raw(&mut peer, r#"{"t":"input","text":"too early"}"#);
    raw(
        &mut peer,
        r#"{"t":"attach","target":"new","workspace":"/w","since":0}"#,
    );
    raw(&mut peer, r#"{"t":"interrupt"}"#);
    raw(&mut peer, r#"{"t":"input","text":"hi"}"#);
    let DaemonMsg::Welcome { caps, .. } = peer.recv() else {
        panic!("no welcome");
    };
    assert_eq!(caps, Caps::NONE);
    assert!(matches!(peer.recv(), DaemonMsg::Error { message } if message == "not attached"));
    let seen = peer
        .until(|m| matches!(m, DaemonMsg::Entry(e) if matches!(e.body, Body::Assistant { .. })));
    assert!(
        seen.iter().all(|m| !matches!(
            m,
            DaemonMsg::Ack { .. } | DaemonMsg::Rejected { .. } | DaemonMsg::Snapshot(_)
        )),
        "{seen:?}"
    );
    assert_eq!(users(&seen), 1);
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
    });
    assert!(matches!(
        peer.recv(),
        DaemonMsg::Welcome { proto: PROTO, .. }
    ));
}

/// A session longer than a snapshot is worth: a client that takes them gets
/// one, an old client the replay.
#[test]
fn far_behind_a_client_gets_a_snapshot_if_it_takes_them() {
    let id = protocol::SessionId::new();
    let fixture = Fixture::with("snapshot", |home| {
        let store = Jsonl::open(home.sessions_dir()).unwrap();
        let created = store
            .create(&Header {
                id,
                workspace: "/w".into(),
                parent: None,
                extensions: Vec::new(),
            })
            .unwrap();
        let (epoch, mut log) = (created.epoch, created.log);
        let entries: Vec<Entry> = (1..=300)
            .map(|seq| {
                let text = format!("m{seq}");
                Entry::new(seq, epoch, Body::User { text })
            })
            .collect();
        log.append_all(&entries).unwrap();
    });
    let replay = |caps| {
        let mut peer = fixture.connect();
        peer.send(&hello(caps));
        peer.send(&attach(Target::Id(id), 10, ClientDecl::default()));
        peer.until(|m| matches!(m, DaemonMsg::Synced { .. }))
    };
    let snapshotted = replay(Caps::ALL);
    let Some(DaemonMsg::Snapshot(view)) = snapshotted.get(2) else {
        panic!("no snapshot: {snapshotted:?}");
    };
    assert_eq!((view.items.len(), view.seq), (300, 301));
    assert_eq!(users(&snapshotted), 0);
    let replayed = replay(Caps::NONE);
    assert!(
        replayed
            .iter()
            .all(|m| !matches!(m, DaemonMsg::Snapshot(_)))
    );
    assert_eq!(users(&replayed), 290);
}

/// A client that stops reading falls behind and is caught up from the log:
/// the daemon holds a bounded backlog for it, and the client still ends up
/// with every entry.
#[test]
fn a_client_that_falls_behind_is_caught_up_from_the_log() {
    let fixture = Fixture::new("behind");
    let attachment = fixture.daemon.resolve(&Target::New, "/w".into()).unwrap();
    let session = attachment.session();
    let outbox = Outbox::new(Caps::NONE);
    session.attach(&outbox, 0);
    let big = "x".repeat(1 << 20);
    for _ in 0..6 {
        session.submit(big.clone()).unwrap();
    }
    fixture.settle();

    let mut view = SessionView::default();
    let mut caught_up = 0;
    while let Some(next) = outbox.try_next() {
        match next {
            Next::Write(frame) => {
                let msg: DaemonMsg = read_frame(&mut frame.as_bytes()).unwrap().unwrap();
                view.fold(&msg);
            }
            Next::CatchUp {
                session: behind,
                following,
                since,
            } => {
                caught_up += 1;
                behind
                    .upgrade()
                    .unwrap()
                    .catch_up(&outbox, following, since);
            }
        }
    }
    let log = fixture
        .daemon
        .shared
        .store
        .load(session.id)
        .unwrap()
        .entries;
    let whole = SessionView::from_log(session.id, "/w".into(), &log);
    assert_eq!(caught_up, 1);
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
