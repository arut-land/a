//! The golden wire corpus: every message shape this build sends, one per
//! file under `fixtures/wire/v<PROTO>`, as every peer of the protocol must
//! read it.
//!
//! The files are written by this test (`UPDATE_WIRE=1 cargo test -p
//! protocol --test wire`), never by hand, and only when missing: a file
//! that exists is what a build of this protocol sent, so it is never
//! rewritten. A change to how a message encodes fails here: it is a new
//! protocol, with a corpus of its own.

// The sample lists are long, one message after another.
#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use protocol::view::SessionView;
use protocol::{
    Activity, Audience, Author, Body, CallId, Caps, Card, ClientDecl, ClientId, ClientMsg,
    ClientSeq, DaemonMsg, Decode, Entry, Environment, HomeSettings, Installed, Mode, Outcome,
    PROTO, Permission, ReadOnly, Rejection, Residency, Role, Seq, SessionId, SessionSummary,
    SettingsChange, Target, Verdict, WaitOn,
};
use serde_json::json;
use uuid::Uuid;

const SESSION: SessionId = SessionId(Uuid::from_u128(0xa));

/// A sample: its file name and the message.
struct Sample<M> {
    name: &'static str,
    msg: M,
}

const fn sample<M>(name: &'static str, msg: M) -> Sample<M> {
    Sample { name, msg }
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
                client: None,
            },
        ),
        sample(
            "hello_range",
            ClientMsg::Hello {
                proto: PROTO + 1,
                build: "0.0.1+0123456789abcdef".into(),
                min: Some(PROTO),
                caps: Caps::NONE,
                client: Some(CLIENT),
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
        sample("declare", ClientMsg::Declare(decl)),
        sample("environment", ClientMsg::Environment(env)),
        sample("input", ClientMsg::input("hi")),
        sample(
            "input_numbered",
            ClientMsg::Input {
                text: "hi".into(),
                client_seq: Some(1),
                mode: Mode::Queue,
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
        sample(
            "steer",
            ClientMsg::Input {
                text: "also this".into(),
                mode: Mode::Steer,
                client_seq: None,
            },
        ),
        sample(
            "steer_numbered",
            ClientMsg::Input {
                text: "also this".into(),
                mode: Mode::Steer,
                client_seq: Some(4),
            },
        ),
        sample("interrupt", ClientMsg::INTERRUPT),
        sample(
            "interrupt_numbered",
            ClientMsg::Interrupt {
                client_seq: Some(3),
            },
        ),
        sample("settings", ClientMsg::Settings { change: None }),
        sample("watch_sessions", ClientMsg::WatchSessions),
        sample(
            "attach_prefix",
            ClientMsg::Attach {
                target: Target::Prefix("0190a".into()),
                workspace: "/w".into(),
                since: 0,
                env: None,
                decl: ClientDecl::default(),
            },
        ),
        sample("status", ClientMsg::Status),
        sample("shutdown", ClientMsg::Shutdown),
        sample(
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
        ),
        sample(
            "hello_client",
            ClientMsg::Hello {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                min: None,
                caps: Caps::NONE,
                client: Some(CLIENT),
            },
        ),
    ]
}

/// A client, as it names itself across its connections.
const CLIENT: ClientId = ClientId(Uuid::from_u128(0xc));

/// The entries about input, decisions, waits and answers.
fn input_samples() -> Vec<Sample<DaemonMsg>> {
    let entry = |seq, body| DaemonMsg::Entry(Entry::new(seq, 0, body));
    let call = |seq| CallId::new(seq).unwrap();
    let seq = |seq| Seq::new(seq).unwrap();
    let author = Author {
        client: CLIENT,
        seq: ClientSeq::new(4).unwrap(),
    };
    vec![
        sample(
            "entry_input",
            entry(
                1,
                Body::Input {
                    text: "hi".into(),
                    mode: Mode::Queue,
                    taken: true,
                    origin: None,
                },
            ),
        ),
        sample(
            "entry_input_steer",
            entry(
                3,
                Body::Input {
                    text: "also this".into(),
                    mode: Mode::Steer,
                    taken: false,
                    origin: None,
                },
            ),
        ),
        sample(
            "entry_input_origin",
            entry(
                3,
                Body::Input {
                    text: "also this".into(),
                    mode: Mode::Queue,
                    taken: false,
                    origin: Some(author),
                },
            ),
        ),
        sample("entry_take", entry(4, Body::Take { input: seq(3) })),
        sample(
            "entry_model_call_attempt",
            entry(
                8,
                Body::ModelCall {
                    model: "openai:m".into(),
                    attempt: 1,
                    choice: protocol::ToolChoice::Auto,
                    r#for: None,
                    purpose: None,
                },
            ),
        ),
        sample(
            "entry_model_call_last",
            entry(
                9,
                Body::ModelCall {
                    model: "openai:m".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::None,
                    r#for: None,
                    purpose: None,
                },
            ),
        ),
        sample(
            "entry_model_call_side",
            entry(
                10,
                Body::ModelCall {
                    model: "openai:m".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: CallId::new(8),
                    purpose: Some("compact".into()),
                },
            ),
        ),
        sample(
            "entry_tool_call_by_user",
            entry(
                22,
                Body::ToolCall {
                    tool: "model".into(),
                    input: json!({ "args": "openai:m" }).into(),
                    id: None,
                    card: None,
                    by: Audience::User,
                },
            ),
        ),
        sample(
            "entry_decision",
            entry(
                12,
                Body::Decision {
                    by: "policy".into(),
                    call: Some(call(2)),
                    d: Verdict::Permission {
                        permission: Permission::Ask,
                    },
                },
            ),
        ),
        sample(
            "entry_decision_retry",
            entry(
                10,
                Body::Decision {
                    by: "retry".into(),
                    call: Some(call(8)),
                    d: Verdict::Retry {
                        attempt: 1,
                        error: "rate limited".into(),
                    },
                },
            ),
        ),
        sample(
            "entry_wait_dialog",
            entry(
                13,
                Body::Wait {
                    call: call(2),
                    on: WaitOn::Dialog {
                        ui: json!({ "title": "pick a file" }).into(),
                    },
                },
            ),
        ),
        sample(
            "entry_answer",
            entry(
                14,
                Body::Answer {
                    call: call(2),
                    value: true.into(),
                    origin: None,
                },
            ),
        ),
        sample(
            "entry_answer_origin",
            entry(
                14,
                Body::Answer {
                    call: call(2),
                    value: false.into(),
                    origin: Some(author),
                },
            ),
        ),
        sample(
            "entry_interrupt_origin",
            entry(
                25,
                Body::Interrupt {
                    origin: Some(author),
                },
            ),
        ),
        sample(
            "entry_edit",
            entry(
                26,
                Body::Edit {
                    target: seq(6),
                    upto: Some(seq(7)),
                    replace: Some(json!("[output elided]").into()),
                },
            ),
        ),
        sample(
            "delta_attempt",
            DaemonMsg::Delta {
                call: 8,
                text: "hel".into(),
                attempt: 1,
            },
        ),
    ]
}

fn daemon_samples() -> Vec<Sample<DaemonMsg>> {
    let entry = |seq, body| DaemonMsg::Entry(Entry::new(seq, 0, body));
    let log = [
        Entry::new(
            1,
            0,
            Body::Input {
                text: "hi".into(),
                mode: Mode::Queue,
                taken: true,
                origin: None,
            },
        ),
        Entry {
            seq: 2,
            epoch: 0,
            body: Body::ToolCall {
                tool: "bash".into(),
                input: json!({ "command": "ls" }).into(),
                id: None,
                card: None,
                by: Audience::Model,
            },
        },
        Entry::new(
            3,
            0,
            Body::Decision {
                by: "policy".into(),
                call: CallId::new(2),
                d: Verdict::Permission {
                    permission: Permission::Ask,
                },
            },
        ),
        Entry::new(
            4,
            0,
            Body::Input {
                text: "and sizes".into(),
                mode: Mode::Steer,
                taken: false,
                origin: None,
            },
        ),
    ];
    vec![
        sample(
            "welcome",
            DaemonMsg::Welcome {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                caps: Caps::NONE,
                commands: Vec::new(),
            },
        ),
        sample(
            "welcome_commands",
            DaemonMsg::Welcome {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                caps: Caps::NONE,
                commands: vec![protocol::CommandInfo {
                    name: "model".into(),
                    description: "show or set the model".into(),
                }],
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
                    by: Audience::Model,
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
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: None,
                    purpose: None,
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
        sample("entry_interrupt", entry(25, Body::INTERRUPT)),
        sample(
            "delta",
            DaemonMsg::Delta {
                call: 4,
                text: "hel".into(),
                attempt: 0,
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
                        read_only: None,
                    },
                    SessionSummary {
                        id: SessionId(Uuid::from_u128(0xb)),
                        workspace: "/v".into(),
                        entries: 9,
                        clients: 0,
                        activity: Activity::Idle,
                        residency: Residency::Hibernated,
                        read_only: None,
                    },
                ],
            },
        ),
        sample(
            "sessions_read_only",
            DaemonMsg::Sessions {
                sessions: [
                    ReadOnly::Damaged { seq: 4 },
                    ReadOnly::Format { version: 2 },
                ]
                .map(|why| SessionSummary {
                    id: SESSION,
                    workspace: "/w".into(),
                    entries: 3,
                    clients: 0,
                    activity: Activity::Idle,
                    residency: Residency::Loaded,
                    read_only: Some(why),
                })
                .into(),
            },
        ),
        sample(
            "error",
            DaemonMsg::Error {
                message: "not attached".into(),
            },
        ),
        sample(
            "snapshot",
            DaemonMsg::Snapshot(Box::new(SessionView::from_log(SESSION, "/w".into(), &log))),
        ),
        sample("ack", DaemonMsg::Ack { client_seq: 2 }),
        sample(
            "rejected",
            DaemonMsg::Rejected {
                client_seq: 2,
                reason: Rejection::NotWaiting,
                message: "call 9 is not waiting for approval".into(),
            },
        ),
        sample(
            "settings",
            DaemonMsg::Settings {
                settings: HomeSettings {
                    model: Some("openai:gpt-6-luna".into()),
                    agent: None,
                    env: BTreeMap::from([("OPENAI_API_KEY".into(), "sk-x".into())]),
                },
                error: None,
            },
        ),
        sample(
            "status",
            DaemonMsg::Status {
                pid: 4242,
                build: "0.0.0+0123456789abcdef".into(),
                started: 1_760_000_000,
                clients: 2,
                known: 7,
            },
        ),
        sample(
            "settings_refused",
            DaemonMsg::Settings {
                settings: HomeSettings::default(),
                error: Some("not a variable name: \"A B\"".into()),
            },
        ),
    ]
    .into_iter()
    .chain(input_samples())
    .collect()
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

/// Whatever a build of this protocol sent, this one reads.
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

/// Capabilities this build does not know are dropped, and a hello says
/// none when it has none.
#[test]
fn capabilities_and_versions_negotiate() {
    let caps: Caps = serde_json::from_str(r#"["from_a_newer_build"]"#).unwrap();
    assert_eq!(caps, Caps::NONE);
    assert_eq!(serde_json::to_string(&caps).unwrap(), "[]");
    let hello = ClientMsg::Hello {
        proto: PROTO,
        build: "b".into(),
        min: None,
        caps,
        client: None,
    };
    assert_eq!(
        serde_json::to_string(&hello).unwrap(),
        format!(r#"{{"t":"hello","proto":{PROTO},"build":"b"}}"#)
    );

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
        r#"{"id":"00000000-0000-0000-0000-00000000000a","workspace":"/w","entries":1,"clients":0,"activity":"soon","residency":"elsewhere"}"#,
    )
    .unwrap();
    assert_eq!(
        (summary.activity, summary.residency),
        (Activity::Other, Residency::Other)
    );
}
