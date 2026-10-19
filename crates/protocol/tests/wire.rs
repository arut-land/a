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
    Activity, Body, Cap, Caps, Card, ClientDecl, ClientMsg, DaemonMsg, Decision, Decode, Entry,
    Environment, HomeSettings, Installed, Origin, Outcome, Overrides, PROTO, Permission, Recap,
    Rejection, Residency, Role, SessionId, SessionSummary, SettingsChange, Target,
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

/// [`first_caps`] and [`Cap::Steer`].
fn steer_caps() -> Caps {
    [Cap::Snapshot, Cap::Ack, Cap::Declare, Cap::Steer]
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
            "hello_caps_steer",
            ClientMsg::Hello {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                min: None,
                caps: steer_caps(),
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
        gated(
            "steer",
            ClientMsg::Steer {
                text: "also this".into(),
                client_seq: None,
            },
            Cap::Steer,
        ),
        gated(
            "steer_numbered",
            ClientMsg::Steer {
                text: "also this".into(),
                client_seq: Some(4),
            },
            Cap::Steer,
        ),
        sample("interrupt", ClientMsg::INTERRUPT),
        sample(
            "interrupt_numbered",
            ClientMsg::Interrupt {
                client_seq: Some(3),
            },
        ),
        gated(
            "settings",
            ClientMsg::Settings { change: None },
            Cap::Settings,
        ),
        gated("watch_sessions", ClientMsg::WatchSessions, Cap::Sessions),
        gated(
            "attach_prefix",
            ClientMsg::Attach {
                target: Target::Prefix("0190a".into()),
                workspace: "/w".into(),
                since: 0,
                env: None,
                decl: ClientDecl::default(),
            },
            Cap::Sessions,
        ),
        gated("status", ClientMsg::Status, Cap::Daemon),
        gated("shutdown", ClientMsg::Shutdown, Cap::Daemon),
        gated(
            "settings_change",
            ClientMsg::Settings {
                change: Some(SettingsChange {
                    model: Some("openai:gpt-6-luna".into()),
                    agent: Some(String::new()),
                    env: BTreeMap::from([
                        ("OPENAI_API_KEY".into(), Some("sk-x".into())),
                        ("OLD".into(), None),
                    ]),
                }),
            },
            Cap::Settings,
        ),
    ]
}

fn daemon_samples() -> Vec<Sample<DaemonMsg>> {
    let entry = |seq, body| DaemonMsg::Entry(Entry::new(seq, 0, body));
    let log = [
        Entry::new(1, 0, Body::User { text: "hi".into() }),
        Entry {
            seq: 2,
            epoch: 0,
            uuid: None,
            parent: None,
            body: Body::ToolCall {
                tool: "bash".into(),
                input: json!({ "command": "ls" }).into(),
                id: None,
                card: None,
            },
        },
        Entry::new(3, 0, Body::Ask { call: 2 }),
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
            "welcome_caps_steer",
            DaemonMsg::Welcome {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                caps: steer_caps(),
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
                    input: json!({ "command": "ls" }).into(),
                    id: Some("call_1".into()),
                    card: Some(Card {
                        title: "$ ls".into(),
                        ..Card::default()
                    }),
                },
            ),
        ),
        sample(
            "entry_queued",
            entry(
                3,
                Body::Queued {
                    text: "also this".into(),
                    steer: true,
                },
            ),
        ),
        sample(
            "entry_steer",
            entry(
                4,
                Body::Steer {
                    text: "also this".into(),
                    queued: 3,
                },
            ),
        ),
        sample(
            "entry_tool_process",
            entry(
                5,
                Body::ToolProcess {
                    call: 2,
                    pgid: 4242,
                    boot: "b".into(),
                    background: true,
                },
            ),
        ),
        sample(
            "entry_progress",
            entry(
                6,
                Body::Progress {
                    call: 2,
                    text: "src\n".into(),
                },
            ),
        ),
        sample(
            "entry_tool_result",
            entry(
                7,
                Body::ToolResult {
                    call: 2,
                    outcome: Outcome::Done {
                        output: json!({ "exit": 0, "stdout": "src\n", "stderr": "" }).into(),
                    },
                    card: Some(Card {
                        title: "$ ls".into(),
                        body: "src\n".into(),
                        note: "exit 0".into(),
                    }),
                },
            ),
        ),
        sample(
            "entry_model_call",
            entry(
                8,
                Body::ModelCall {
                    model: "openai:m".into(),
                },
            ),
        ),
        sample(
            "entry_model_result_failed",
            entry(
                9,
                Body::ModelResult {
                    call: 8,
                    outcome: Outcome::Failed {
                        error: "rate limited".into(),
                    },
                },
            ),
        ),
        sample(
            "entry_model_result_interrupted",
            entry(
                10,
                Body::ModelResult {
                    call: 8,
                    outcome: Outcome::Interrupted {
                        output: Some("so far".into()),
                    },
                },
            ),
        ),
        sample(
            "entry_assistant",
            entry(
                11,
                Body::Assistant {
                    text: "done".into(),
                },
            ),
        ),
        sample(
            "entry_hook",
            entry(
                12,
                Body::Hook {
                    hook: "policy".into(),
                    decision: Decision::Permission {
                        tool: "bash".into(),
                        permission: Permission::Ask,
                    },
                },
            ),
        ),
        sample("entry_ask", entry(13, Body::Ask { call: 2 })),
        sample(
            "entry_approval",
            entry(
                14,
                Body::Hook {
                    hook: "user".into(),
                    decision: Decision::Approval {
                        tool: "bash".into(),
                        approved: true,
                        call: Some(2),
                    },
                },
            ),
        ),
        sample(
            "entry_link",
            entry(
                15,
                Body::Link {
                    child: SessionId(Uuid::from_u128(0xc)),
                    input: "look".into(),
                },
            ),
        ),
        sample(
            "entry_watch",
            entry(
                16,
                Body::Watch {
                    child: SessionId(Uuid::from_u128(0xc)),
                },
            ),
        ),
        sample(
            "entry_fired",
            entry(
                17,
                Body::Fired {
                    watch: 16,
                    outcome: Outcome::Done {
                        output: json!({ "text": "found it" }).into(),
                    },
                },
            ),
        ),
        sample(
            "entry_environment",
            entry(
                18,
                Body::Environment(Environment {
                    cwd: "/w".into(),
                    vars: BTreeMap::from([("PATH".into(), "/bin".into())]),
                    redacted: vec!["GITHUB_TOKEN".into()],
                    tty: false,
                }),
            ),
        ),
        sample(
            "entry_extensions",
            entry(
                19,
                Body::Extensions {
                    installed: vec![Installed {
                        name: "shell".into(),
                        version: 1,
                    }],
                },
            ),
        ),
        sample(
            "entry_ext",
            entry(
                20,
                Body::Ext {
                    ext: "shell".into(),
                    state: "cwd".into(),
                    data: json!("/w").into(),
                },
            ),
        ),
        sample(
            "entry_context",
            entry(
                21,
                Body::Context {
                    text: "cwd: /w".into(),
                },
            ),
        ),
        sample(
            "entry_command",
            entry(
                22,
                Body::Command {
                    name: "model".into(),
                    args: "openai:m".into(),
                },
            ),
        ),
        sample(
            "entry_command_result",
            entry(
                23,
                Body::CommandResult {
                    call: 22,
                    outcome: Outcome::Done {
                        output: json!(null).into(),
                    },
                },
            ),
        ),
        sample(
            "entry_settings",
            entry(
                24,
                Body::Settings(Overrides {
                    model: Some("openai:m".into()),
                }),
            ),
        ),
        sample("entry_interrupt", entry(25, Body::Interrupt)),
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
                input: json!({ "command": "ls" }).into(),
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
        gated(
            "settings",
            DaemonMsg::Settings {
                settings: HomeSettings {
                    model: Some("openai:gpt-6-luna".into()),
                    agent: None,
                    env: BTreeMap::from([("OPENAI_API_KEY".into(), "sk-x".into())]),
                },
                error: None,
            },
            Cap::Settings,
        ),
        gated(
            "status",
            DaemonMsg::Status {
                pid: 4242,
                build: "0.0.0+0123456789abcdef".into(),
                started: 1_760_000_000,
                clients: 2,
                known: 7,
            },
            Cap::Daemon,
        ),
        gated(
            "settings_refused",
            DaemonMsg::Settings {
                settings: HomeSettings::default(),
                error: Some("not a variable name: \"A B\"".into()),
            },
            Cap::Settings,
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

/// An entry's frame holds its log line, and decodes back to the entry: a
/// daemon stores, sends and reads again the one encoding.
#[test]
fn an_entry_frame_is_its_log_line_tagged() {
    for sample in daemon_samples() {
        let DaemonMsg::Entry(entry) = &sample.msg else {
            continue;
        };
        let frame = protocol::Frame::entry(entry).unwrap();
        let mut line = Vec::new();
        entry.encode(&mut line).unwrap();
        let read = protocol::Frame::from_line(entry.seq, &line).unwrap();
        assert_eq!(read.as_bytes(), frame.as_bytes(), "{}", sample.name);
        assert_eq!((read.seq, read.class), (frame.seq, frame.class));
        assert_eq!(&Entry::from_json(&frame.as_bytes()[4..]).unwrap(), entry);
    }
}

/// Ungated messages the first daemons cannot read, which no client may
/// send until a capability gates them: those daemons drop a connection on
/// a message they cannot decode. None now; `steer` waits for
/// [`Cap::Steer`].
const UNREAD_BY_THE_FIRST: &[&str] = &[];

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
        if gate(&client, &name).is_none() && !UNREAD_BY_THE_FIRST.contains(&name.as_str()) {
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
