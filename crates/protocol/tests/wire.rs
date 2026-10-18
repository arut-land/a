//! The golden wire corpus: every message shape this build sends, one per
//! file under `fixtures/wire/v<PROTO>`, as old and new peers must read it.
//!
//! The files are written by this test (`UPDATE_WIRE=1 cargo test -p
//! protocol --test wire`), never by hand, and only when missing: a file
//! that exists is what an earlier build sent, so it is never rewritten. A
//! change to how a message encodes fails here; a new field or shape gets a
//! new sample (and file) beside the old one, which must keep decoding.

// The sample lists are long, one message after another.
#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use protocol::view::SessionView;
use protocol::{
    Activity, Body, Cap, Caps, Card, ClientDecl, ClientMsg, DaemonMsg, Decode, Entry, Environment,
    Origin, PROTO, Recap, Rejection, Residency, Role, SessionId, SessionSummary, Target,
};
use serde_json::json;
use uuid::Uuid;

const SESSION: SessionId = SessionId(Uuid::from_u128(0xa));

/// The capabilities of the first build that had any, named rather than
/// `Caps::ALL`, which grows.
fn first_caps() -> Caps {
    [Cap::Snapshot, Cap::Ack, Cap::Declare]
        .into_iter()
        .collect()
}

/// A sample: its file name, the message, and the capability without which
/// it is never sent (so peers older than it never see it).
struct Sample<M> {
    name: &'static str,
    msg: M,
    gate: Option<Cap>,
}

const fn sample<M>(name: &'static str, msg: M) -> Sample<M> {
    Sample {
        name,
        msg,
        gate: None,
    }
}

const fn gated<M>(name: &'static str, msg: M, cap: Cap) -> Sample<M> {
    Sample {
        name,
        msg,
        gate: Some(cap),
    }
}

fn client_samples() -> Vec<Sample<ClientMsg>> {
    let env = Environment {
        cwd: "/w".into(),
        vars: BTreeMap::from([("PATH".into(), "/bin".into())]),
        redacted: vec!["GITHUB_TOKEN".into()],
        tty: true,
    };
    let decl = ClientDecl {
        role: Role::Collaborator,
        tools: vec!["pick_file".into()],
        sites: vec!["dialog".into()],
    };
    vec![
        sample(
            "hello",
            ClientMsg::Hello {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                min: None,
                caps: Caps::NONE,
            },
        ),
        sample(
            "hello_caps",
            ClientMsg::Hello {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                min: None,
                caps: first_caps(),
            },
        ),
        sample(
            "hello_range",
            ClientMsg::Hello {
                proto: PROTO + 1,
                build: "0.0.1+0123456789abcdef".into(),
                min: Some(PROTO),
                caps: first_caps(),
            },
        ),
        sample(
            "attach",
            ClientMsg::Attach {
                target: Target::New,
                workspace: "/w".into(),
                since: 0,
                env: None,
                decl: ClientDecl::default(),
            },
        ),
        sample(
            "attach_latest",
            ClientMsg::Attach {
                target: Target::Latest {
                    any_workspace: true,
                },
                workspace: "/w".into(),
                since: 0,
                env: None,
                decl: ClientDecl::default(),
            },
        ),
        sample(
            "attach_id_env_decl",
            ClientMsg::Attach {
                target: Target::Id(SESSION),
                workspace: "/w".into(),
                since: 7,
                env: Some(env.clone()),
                decl: decl.clone(),
            },
        ),
        gated("declare", ClientMsg::Declare(decl), Cap::Declare),
        sample("environment", ClientMsg::Environment(env)),
        sample("input", ClientMsg::input("hi")),
        sample(
            "input_numbered",
            ClientMsg::Input {
                text: "hi".into(),
                client_seq: Some(1),
            },
        ),
        sample("list", ClientMsg::List),
        sample("detach", ClientMsg::Detach),
        sample("approve", ClientMsg::approve(3, true)),
        sample(
            "approve_numbered",
            ClientMsg::Approve {
                call: 3,
                approved: false,
                client_seq: Some(2),
            },
        ),
        sample("interrupt", ClientMsg::INTERRUPT),
        sample(
            "interrupt_numbered",
            ClientMsg::Interrupt {
                client_seq: Some(3),
            },
        ),
    ]
}

fn daemon_samples() -> Vec<Sample<DaemonMsg>> {
    let entry = |seq, body| {
        DaemonMsg::Entry(Entry {
            seq,
            epoch: 0,
            body,
        })
    };
    let log = [
        Entry {
            seq: 1,
            epoch: 0,
            body: Body::User { text: "hi".into() },
        },
        Entry {
            seq: 2,
            epoch: 0,
            body: Body::ToolCall {
                tool: "bash".into(),
                input: json!({ "command": "ls" }),
                id: None,
                card: None,
            },
        },
        Entry {
            seq: 3,
            epoch: 0,
            body: Body::Ask { call: 2 },
        },
    ];
    let origin = Origin {
        client: 1,
        client_seq: 2,
    };
    vec![
        sample(
            "welcome",
            DaemonMsg::Welcome {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                caps: Caps::NONE,
                client: 0,
            },
        ),
        sample(
            "welcome_caps",
            DaemonMsg::Welcome {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                caps: first_caps(),
                client: 1,
            },
        ),
        sample(
            "mismatch",
            DaemonMsg::Mismatch {
                supported: PROTO,
                min: None,
            },
        ),
        sample(
            "mismatch_range",
            DaemonMsg::Mismatch {
                supported: PROTO + 1,
                min: Some(PROTO),
            },
        ),
        sample("draining", DaemonMsg::Draining),
        sample(
            "attached",
            DaemonMsg::Attached {
                session: SESSION,
                workspace: "/w".into(),
            },
        ),
        sample("entry_user", entry(1, Body::User { text: "hi".into() })),
        sample(
            "entry_tool_call",
            entry(
                2,
                Body::ToolCall {
                    tool: "bash".into(),
                    input: json!({ "command": "ls" }),
                    id: Some("call_1".into()),
                    card: Some(Card {
                        title: "$ ls".into(),
                        ..Card::default()
                    }),
                },
            ),
        ),
        sample(
            "delta",
            DaemonMsg::Delta {
                call: 4,
                text: "hel".into(),
            },
        ),
        sample("synced", DaemonMsg::Synced { seq: 3 }),
        sample(
            "sessions",
            DaemonMsg::Sessions {
                sessions: vec![
                    SessionSummary {
                        id: SESSION,
                        workspace: "/w".into(),
                        entries: 3,
                        clients: 1,
                        activity: Activity::Approval,
                        residency: Residency::Loaded,
                    },
                    SessionSummary {
                        id: SessionId(Uuid::from_u128(0xb)),
                        workspace: "/v".into(),
                        entries: 9,
                        clients: 0,
                        activity: Activity::Idle,
                        residency: Residency::Hibernated,
                    },
                ],
            },
        ),
        sample(
            "recap",
            DaemonMsg::Recap(Recap {
                prompt: Some("hi".into()),
                tools: vec!["bash".into()],
                failed: 0,
                running: 1,
                reply: None,
                asks: 1,
            }),
        ),
        sample(
            "ask",
            DaemonMsg::Ask {
                call: 2,
                tool: "bash".into(),
                input: json!({ "command": "ls" }),
            },
        ),
        sample(
            "error",
            DaemonMsg::Error {
                message: "not attached".into(),
            },
        ),
        gated(
            "snapshot",
            DaemonMsg::Snapshot(Box::new(SessionView::from_log(SESSION, "/w".into(), &log))),
            Cap::Snapshot,
        ),
        gated("ack", DaemonMsg::Ack { origin }, Cap::Ack),
        gated(
            "rejected",
            DaemonMsg::Rejected {
                origin,
                reason: Rejection::NotWaiting,
                message: "call 9 is not waiting for approval".into(),
            },
            Cap::Ack,
        ),
    ]
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../fixtures/wire/v{PROTO}"))
}

/// Each sample's file holds its encoding: written when missing under
/// `UPDATE_WIRE=1`, never rewritten.
fn check<M: serde::Serialize>(dir: &str, samples: &[Sample<M>]) {
    let dir = corpus().join(dir);
    let update = std::env::var_os("UPDATE_WIRE").is_some();
    let mut missing = Vec::new();
    for s in samples {
        let path = dir.join(format!("{}.json", s.name));
        let encoded = serde_json::to_string(&s.msg).unwrap() + "\n";
        match fs::read_to_string(&path) {
            Ok(stored) => assert_eq!(
                stored,
                encoded,
                "{} encodes differently from the corpus: a peer that reads the old \
                 form breaks; add a new sample instead",
                path.display()
            ),
            Err(_) if update => {
                fs::create_dir_all(&dir).unwrap();
                fs::write(&path, encoded).unwrap();
            }
            Err(_) => missing.push(s.name),
        }
    }
    assert!(
        missing.is_empty(),
        "not in the corpus: {missing:?}; UPDATE_WIRE=1 adds them"
    );
}

/// Every file in `dir`, by name.
fn files(dir: &str) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = fs::read_dir(corpus().join(dir))
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            (name, fs::read(path).unwrap())
        })
        .collect();
    files.sort();
    files
}

#[test]
fn every_message_this_build_sends_is_in_the_corpus() {
    check("client", &client_samples());
    check("daemon", &daemon_samples());
}

/// Whatever any build ever sent, this one reads.
#[test]
fn this_build_reads_the_whole_corpus() {
    for (name, bytes) in files("client") {
        ClientMsg::from_json(&bytes).unwrap_or_else(|e| panic!("client/{name}: {e}"));
    }
    for (name, bytes) in files("daemon") {
        let msg = DaemonMsg::from_json(&bytes).unwrap_or_else(|e| panic!("daemon/{name}: {e}"));
        assert!(
            !matches!(msg, DaemonMsg::Unknown { .. }),
            "daemon/{name} is unknown"
        );
    }
}

/// The first protocol 1 builds read every message sent to them: anything
/// not gated by a capability, which they never ask for.
#[test]
fn the_first_builds_read_what_is_sent_to_them() {
    let gate = |samples: &[(&str, Option<Cap>)], name: &str| {
        samples
            .iter()
            .find(|(n, _)| *n == name)
            .and_then(|(_, gate)| *gate)
    };
    let client: Vec<_> = client_samples().iter().map(|s| (s.name, s.gate)).collect();
    let daemon: Vec<_> = daemon_samples().iter().map(|s| (s.name, s.gate)).collect();
    for (name, bytes) in files("client") {
        if gate(&client, &name).is_none() {
            serde_json::from_slice::<first::ClientMsg>(&bytes)
                .unwrap_or_else(|e| panic!("client/{name}: {e}"));
        }
    }
    for (name, bytes) in files("daemon") {
        if gate(&daemon, &name).is_none() {
            serde_json::from_slice::<first::DaemonMsg>(&bytes)
                .unwrap_or_else(|e| panic!("daemon/{name}: {e}"));
        }
    }
}

/// The messages as the first protocol 1 builds (up to 5d37d48) declared
/// them, without the fields and kinds added since.
#[allow(dead_code)]
mod first {
    use std::path::PathBuf;

    use protocol::{Entry, Environment, Recap, SessionId, Target};
    use serde::Deserialize;
    use serde_json::Value;

    #[derive(Deserialize)]
    #[serde(tag = "t", rename_all = "snake_case")]
    pub enum ClientMsg {
        Hello {
            proto: u32,
            build: String,
        },
        Attach {
            target: Target,
            workspace: PathBuf,
            since: u64,
            #[serde(default)]
            env: Option<Environment>,
        },
        Environment(Environment),
        Input {
            text: String,
        },
        List,
        Detach,
        Approve {
            call: u64,
            approved: bool,
        },
        Interrupt,
    }

    #[derive(Deserialize)]
    pub struct SessionSummary {
        pub id: SessionId,
        pub workspace: PathBuf,
        pub entries: u64,
        pub clients: u32,
    }

    #[derive(Deserialize)]
    #[serde(tag = "t", rename_all = "snake_case")]
    pub enum DaemonMsg {
        Welcome {
            proto: u32,
            build: String,
        },
        Mismatch {
            supported: u32,
        },
        Draining,
        Attached {
            session: SessionId,
            workspace: PathBuf,
        },
        Entry(Entry),
        Delta {
            call: u64,
            text: String,
        },
        Synced {
            seq: u64,
        },
        Sessions {
            sessions: Vec<SessionSummary>,
        },
        Recap(Recap),
        Ask {
            call: u64,
            tool: String,
            input: Value,
        },
        Error {
            message: String,
        },
    }
}

#[test]
fn capabilities_and_versions_negotiate() {
    let caps: Caps = serde_json::from_str(r#"["ack","from_a_newer_build","snapshot"]"#).unwrap();
    assert!(caps.has(Cap::Ack) && caps.has(Cap::Snapshot) && !caps.has(Cap::Declare));
    assert_eq!(
        serde_json::to_string(&caps).unwrap(),
        r#"["snapshot","ack"]"#
    );
    let theirs: Caps = [Cap::Ack].into_iter().collect();
    assert_eq!((caps & theirs).iter().collect::<Vec<_>>(), [Cap::Ack]);

    let v = protocol::Versions::new;
    assert_eq!(v(None, 1).common(v(None, 1)), Some(1));
    assert_eq!(v(Some(1), 3).common(v(Some(2), 5)), Some(3));
    assert_eq!(v(None, 1).common(v(Some(2), 3)), None);
    assert_eq!(v(Some(2), 3).to_string(), "protocols 2-3");
}

#[test]
fn values_from_a_newer_build_read_as_their_safest() {
    let role: Role = serde_json::from_str(r#""admin""#).unwrap();
    assert_eq!(role, Role::Viewer);
    let activity: Activity = serde_json::from_str(r#""migrating_soon""#).unwrap();
    assert_eq!(activity, Activity::Other);
    let reason: Rejection = serde_json::from_str(r#""quota""#).unwrap();
    assert_eq!(reason, Rejection::Other);
    let summary: SessionSummary = serde_json::from_str(
        r#"{"id":"00000000-0000-0000-0000-00000000000a","workspace":"/w","entries":1,"clients":0}"#,
    )
    .unwrap();
    assert_eq!(
        (summary.activity, summary.residency),
        (Activity::Idle, Residency::Hibernated)
    );
}
