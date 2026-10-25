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
    ClientSeq, DaemonMsg, Decode, Entry, Environment, Epoch, HomeSettings, Installed, MachineId,
    Mode, Outcome, PROTO, Permission, ReadOnly, Rejection, Residency, Seq, SessionId,
    SessionSummary, SettingsChange, Target, Verdict, WaitOn,
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

fn envelope() -> protocol::Envelope {
    protocol::Envelope {
        id: protocol::EnvelopeId {
            sender: SESSION,
            call: CallId::new(3).unwrap(),
            purpose: "subagent:message".into(),
        },
        destination: SessionId(Uuid::from_u128(0xc)),
        payload: "hello".into(),
        held: Vec::new(),
    }
}

/// Input of every segment kind.
fn segments() -> protocol::Segments {
    use protocol::{BlobId, BlobOrigin, BlobRef, Segment};
    let blob = |media_type: &str, origin| BlobRef {
        id: BlobId([0x0f; 32]),
        size: 9000,
        media_type: media_type.into(),
        origin,
    };
    protocol::Segments(vec![
        Segment::Text("see ".into()),
        Segment::Paste("a\nb".into()),
        Segment::PasteRef(blob("text/plain", BlobOrigin::Paste)),
        Segment::Attachment(blob(
            "image/png",
            BlobOrigin::Attachment {
                name: "shot.png".into(),
            },
        )),
    ])
}

fn context_image() -> protocol::publication::ContextImage {
    use protocol::publication::{
        ContextInput, ContextLayer, ContextValue, Publication, RunnerKind,
    };
    let values = [
        ContextValue::Inherited(1),
        ContextValue::User("summary at an excluded position".into()),
        ContextValue::Queued("consumed input".into(), Mode::Steer),
        ContextValue::Inbox(envelope()),
        ContextValue::Taken(Seq::new(3).unwrap()),
        ContextValue::Assistant("answer".into()),
        ContextValue::Context(
            "published context".into(),
            Some(Publication {
                namespace: "skills".into(),
                key: protocol::Json::encode("review").unwrap(),
            }),
        ),
        ContextValue::Child(Outcome::Interrupted { output: None }),
        ContextValue::Into(Seq::new(3).unwrap(), CallId::new(9).unwrap()),
        ContextValue::Withdrawn(Seq::new(3).unwrap()),
        ContextValue::Detached(CallId::new(9).unwrap()),
        ContextValue::Background(
            CallId::new(9).unwrap(),
            Outcome::Interrupted { output: None },
            Vec::new(),
        ),
        ContextValue::Reply(protocol::Json::null()),
        ContextValue::ToolCall("provider-id".into(), "example".into()),
        ContextValue::ToolResult(
            CallId::new(9).unwrap(),
            Outcome::Interrupted { output: None },
            Vec::new(),
        ),
    ];
    let inputs: Vec<_> = values
        .into_iter()
        .zip(1..)
        .map(|(value, seq)| ContextInput(Seq::new(seq).unwrap(), value))
        .collect();
    vec![
        ContextLayer(RunnerKind::Agent, inputs),
        ContextLayer(
            RunnerKind::ModelOnly,
            vec![ContextInput(
                Seq::new(1).unwrap(),
                ContextValue::User("ancestor".into()),
            )],
        ),
    ]
}

fn client_samples() -> Vec<Sample<ClientMsg>> {
    let env = Environment {
        cwd: "/w".into(),
        vars: BTreeMap::from([("PATH".into(), "/bin".into())]),
        redacted: vec!["GITHUB_TOKEN".into()],
        tty: true,
    };
    let decl = ClientDecl {
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
                machine: None,
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
                machine: Some(MACHINE),
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
            "attach_fork",
            ClientMsg::Attach {
                target: Target::Fork {
                    session: SESSION.to_string(),
                    seq: Seq::new(7),
                },
                workspace: "/ignored".into(),
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
                segments: "hi".into(),
                client_seq: Some(1),
                mode: Mode::Queue,
                force: false,
            },
        ),
        sample("input_segments", ClientMsg::input(segments())),
        sample("list", ClientMsg::List),
        sample("detach", ClientMsg::Detach),
        sample("approve", ClientMsg::approve(CallId::new(3).unwrap(), true)),
        sample(
            "approve_numbered",
            ClientMsg::Approve {
                call: CallId::new(3).unwrap(),
                approved: false,
                client_seq: Some(2),
            },
        ),
        sample(
            "steer",
            ClientMsg::Input {
                segments: "also this".into(),
                mode: Mode::Steer,
                client_seq: None,
                force: false,
            },
        ),
        sample(
            "steer_numbered",
            ClientMsg::Input {
                segments: "also this".into(),
                mode: Mode::Steer,
                client_seq: Some(4),
                force: false,
            },
        ),
        sample(
            "steer_forced",
            ClientMsg::Input {
                segments: "now".into(),
                mode: Mode::Steer,
                client_seq: Some(5),
                force: true,
            },
        ),
        sample(
            "withdraw",
            ClientMsg::Withdraw {
                input: Seq::new(3).unwrap(),
                client_seq: Some(6),
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
        sample(
            "query_daemon",
            ClientMsg::Query {
                id: 1,
                service: "models".into(),
                scope: protocol::Scope::Daemon,
                body: protocol::Json::null(),
            },
        ),
        sample(
            "query_workspace",
            ClientMsg::Query {
                id: 2,
                service: "files.search".into(),
                scope: protocol::Scope::Workspace("/work/a/src".into()),
                body: protocol::Json::encode(&json!({"query": "lib", "limit": 20})).unwrap(),
            },
        ),
        sample(
            "query_session",
            ClientMsg::Query {
                id: 3,
                service: "files.search".into(),
                scope: protocol::Scope::Session(SESSION),
                body: protocol::Json::encode(&json!({"query": "lib"})).unwrap(),
            },
        ),
        sample("cancel", ClientMsg::Cancel { id: 2 }),
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
                machine: None,
            },
        ),
    ]
}

/// A client, as it names itself across its connections.
const CLIENT: ClientId = ClientId(Uuid::from_u128(0xc));

/// A machine, as a welcome names the daemon's.
const MACHINE: MachineId = MachineId(*b"machine-id-bytes");

/// The entries about input, decisions, waits and answers.
fn input_samples() -> Vec<Sample<DaemonMsg>> {
    let entry = |seq, body| DaemonMsg::Entry(Entry::new(Seq::new(seq).unwrap(), Epoch(0), body));
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
                    segments: "hi".into(),
                    mode: Mode::Queue,
                    taken: true,
                    origin: None,
                    force: false,
                },
            ),
        ),
        sample(
            "entry_input_segments",
            entry(
                2,
                Body::Input {
                    segments: segments(),
                    mode: Mode::Queue,
                    taken: true,
                    origin: None,
                    force: false,
                },
            ),
        ),
        sample(
            "entry_input_steer",
            entry(
                3,
                Body::Input {
                    segments: "also this".into(),
                    mode: Mode::Steer,
                    taken: false,
                    origin: None,
                    force: false,
                },
            ),
        ),
        sample(
            "entry_input_origin",
            entry(
                3,
                Body::Input {
                    segments: "also this".into(),
                    mode: Mode::Queue,
                    taken: false,
                    origin: Some(author),
                    force: false,
                },
            ),
        ),
        sample(
            "entry_take",
            entry(
                4,
                Body::Take {
                    input: seq(3),
                    into: None,
                },
            ),
        ),
        sample(
            "entry_model_call_attempt",
            entry(
                8,
                Body::ModelCall {
                    request: None,
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
                    request: None,
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
                    request: Some(
                        protocol::Json::encode(&json!({"opaque": "side intent"})).unwrap(),
                    ),
                    model: "openai:m".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: CallId::new(8),
                    purpose: Some("compact".into()),
                },
            ),
        ),
        sample(
            "entry_call_fact",
            DaemonMsg::Entry(Entry {
                seq: Seq::new(11).unwrap(),
                epoch: Epoch(0),
                execution: Some(
                    (protocol::CallFact {
                        id: protocol::AttemptId {
                            executing_session: SESSION,
                            call: call(8),
                            attempt: 1,
                        },
                        model: "openai:m".into(),
                        role: "session".into(),
                        runner: "agent".into(),
                        agent: "loop".into(),
                        side: false,
                        unix_ms: 1000,
                        clock_epoch: SESSION,
                        start_ns: 10,
                        elapsed_ns: Some(20),
                        usage: Some(
                            protocol::Json::encode(&[
                                Some(10),
                                None,
                                None,
                                None,
                                None,
                                None,
                                None,
                                None,
                                None,
                                None,
                            ])
                            .unwrap(),
                        ),
                        charge: Some(protocol::Charge {
                            currency: "USD".into(),
                            nanos: 100,
                        }),
                    })
                    .to_json()
                    .unwrap(),
                ),
                body: Body::ModelResult {
                    call: call(8),
                    r#for: None,
                    outcome: Outcome::Interrupted { output: None },
                    failure: None,
                },
            }),
        ),
        sample(
            "entry_annotation",
            entry(
                11,
                Body::Fact {
                    fact: protocol::ExtensionFact::Annotation(protocol::Annotation {
                        target: call(8),
                        lifetime: protocol::AnnotationLifetime::Question,
                        card: Card {
                            body: "Optional extension explanation".into(),
                            ..Card::default()
                        },
                    }),
                },
            ),
        ),
        sample(
            "entry_tool_call_by_user",
            entry(
                22,
                Body::ToolCall {
                    invoke: None,
                    tool: "model".into(),
                    input: protocol::Json::encode(&json!({ "args": "openai:m" })).unwrap(),
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
                        permission: Permission::Ask(protocol::ApprovalReason::Mode {
                            mode: "ask".into(),
                        }),
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
                        ui: protocol::Json::encode(&json!({ "title": "pick a file" })).unwrap(),
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
                    value: protocol::Json::encode(&true).unwrap(),
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
                    value: protocol::Json::encode(&false).unwrap(),
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
                    replace: Some(protocol::Json::encode(&json!("[output elided]")).unwrap()),
                },
            ),
        ),
        sample(
            "delta_attempt",
            DaemonMsg::Delta {
                call: CallId::new(8).unwrap(),
                text: "hel".into(),
                attempt: 1,
            },
        ),
    ]
}

fn daemon_samples() -> Vec<Sample<DaemonMsg>> {
    let entry = |seq, body| DaemonMsg::Entry(Entry::new(Seq::new(seq).unwrap(), Epoch(0), body));
    let log = [
        Entry::new(
            Seq::new(1).unwrap(),
            Epoch(0),
            Body::Input {
                segments: "hi".into(),
                mode: Mode::Queue,
                taken: true,
                origin: None,
                force: false,
            },
        ),
        Entry {
            seq: Seq::new(2).unwrap(),
            epoch: Epoch(0),
            execution: None,
            body: Body::ToolCall {
                invoke: None,
                tool: "bash".into(),
                input: protocol::Json::encode(&json!({ "command": "ls" })).unwrap(),
                id: None,
                card: None,
                by: Audience::Model,
            },
        },
        Entry::new(
            Seq::new(3).unwrap(),
            Epoch(0),
            Body::Decision {
                by: "policy".into(),
                call: CallId::new(2),
                d: Verdict::Permission {
                    permission: Permission::Ask(protocol::ApprovalReason::Mode {
                        mode: "ask".into(),
                    }),
                },
            },
        ),
        Entry::new(
            Seq::new(4).unwrap(),
            Epoch(0),
            Body::Input {
                segments: "and sizes".into(),
                mode: Mode::Steer,
                taken: false,
                origin: None,
                force: false,
            },
        ),
    ];
    vec![
        sample(
            "entry_side_retry",
            entry(
                12,
                Body::Decision {
                    by: "retry".into(),
                    call: CallId::new(10),
                    d: Verdict::SideRetry {
                        attempt: 1,
                        failure: protocol::Json::encode(&json!({"opaque": "model failure"}))
                            .unwrap(),
                        after_ms: 2000,
                    },
                },
            ),
        ),
        sample(
            "welcome",
            DaemonMsg::Welcome {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                caps: Caps::NONE,
                machine: MACHINE,
                commands: Vec::new(),
                queries: Vec::new(),
            },
        ),
        sample(
            "welcome_commands",
            DaemonMsg::Welcome {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                caps: Caps::NONE,
                machine: MACHINE,
                commands: vec![protocol::CommandInfo {
                    name: "model".into(),
                    description: "show or set the model".into(),
                }],
                queries: Vec::new(),
            },
        ),
        sample(
            "welcome_queries",
            DaemonMsg::Welcome {
                proto: PROTO,
                build: "0.0.0+0123456789abcdef".into(),
                caps: Caps::NONE,
                machine: MACHINE,
                commands: Vec::new(),
                queries: vec![protocol::QueryInfo {
                    name: "files.search".into(),
                    description: "the workspace's files that match".into(),
                    scope: protocol::ScopeKind::Workspace,
                }],
            },
        ),
        sample(
            "update",
            DaemonMsg::Update {
                id: 7,
                body: protocol::Json::encode(&json!({"path": "src/lib.rs"})).unwrap(),
            },
        ),
        sample(
            "reply",
            DaemonMsg::Reply {
                id: 7,
                outcome: Outcome::Done {
                    output: protocol::Json::encode(&json!(["src/lib.rs"])).unwrap(),
                },
            },
        ),
        sample(
            "reply_failed",
            DaemonMsg::Reply {
                id: 7,
                outcome: Outcome::Failed {
                    error: "no query named files.grep".into(),
                },
            },
        ),
        sample(
            "reply_cancelled",
            DaemonMsg::Reply {
                id: 7,
                outcome: Outcome::Interrupted { output: None },
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
        sample("entry_tool_invocation", entry(3, Body::ToolCall {
            invoke: Some(protocol::Invocation { parent: CallId::new(2).unwrap(), name: "lookup".into() }),
            tool: "bash".into(), input: protocol::Json::encode(&json!({"command":"ls"})).unwrap(),
            id: None, card: None, by: Audience::Model,
        })),
        sample(
            "entry_tool_call",
            entry(
                2,
                Body::ToolCall {
                    invoke: None,
                    tool: "bash".into(),
                    input: protocol::Json::encode(&json!({ "command": "ls" })).unwrap(),
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
                    call: CallId::new(2).unwrap(),
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
                    call: CallId::new(2).unwrap(),
                    text: "src\n".into(),
                },
            ),
        ),
        sample(
            "entry_tool_result",
            entry(
                7,
                Body::ToolResult {
                    call: CallId::new(2).unwrap(),
                    outcome: Outcome::Done {
                        output: protocol::Json::encode(
                            &json!({ "exit": 0, "stdout": "src\n", "stderr": "" }),
                        )
                        .unwrap(),
                    },
                    card: Some(Card {
                        title: "$ ls".into(),
                        body: "src\n".into(),
                        note: "exit 0".into(),
                        ..Card::default()
                    }), held: Vec::new(),
                },
            ),
        ),
        sample(
            "entry_model_call",
            entry(
                8,
                Body::ModelCall {
                    request: None,
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
                    r#for: Some(CallId::new(4).unwrap()),
                    failure: Some(
                        protocol::Json::encode(&json!({"opaque": "model failure"})).unwrap(),
                    ),
                    call: CallId::new(8).unwrap(),
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
                    r#for: None,
                    failure: None,
                    call: CallId::new(8).unwrap(),
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
                Body::Link { intent: protocol::Json::null(),
                    child: SessionId(Uuid::from_u128(0xc)),
                    input: "look".into(),
                },
            ),
        ),
        sample("entry_outbox", entry(1, Body::Outbox { envelope_id: envelope().id, recipients: vec![envelope().destination, SessionId(Uuid::from_u128(0xd))], payload: "hello".into() })),
        sample("entry_enqueue", entry(1, Body::Enqueue { envelope: envelope() })),
        sample("entry_enqueue_detached", entry(19, Body::Enqueue { envelope: protocol::Envelope::own(SESSION, CallId::new(4).unwrap(), protocol::EnvelopeId::DETACHED, Outcome::Interrupted { output: None }.payload()).holding(vec![protocol::Held::Blob(protocol::BlobRef { id: protocol::BlobId([0x0f; 32]), size: 9000, media_type: "text/plain".into(), origin: protocol::BlobOrigin::ToolOutput(CallId::new(4).unwrap()) })]) })),
        sample("entry_enqueue_own", entry(18, Body::Enqueue { envelope: protocol::Envelope::own(SESSION, CallId::new(16).unwrap(), protocol::EnvelopeId::CHILD, Outcome::Interrupted { output: None }.payload()) })),
        sample("entry_ack", entry(2, Body::Ack { envelope_id: envelope().id, recipient: envelope().destination, refusal: None })),
        sample("entry_inbox_full", entry(2, Body::Ack { envelope_id: envelope().id, recipient: envelope().destination, refusal: Some(protocol::InboxRefusal::Full) })),
        sample("entry_withdraw", entry(5, Body::Withdraw { input: Seq::new(3).unwrap(), origin: None })),
        sample("entry_take_into", entry(6, Body::Take { input: Seq::new(3).unwrap(), into: CallId::new(4) })),
        sample("entry_detached", entry(7, Body::Detached { call: CallId::new(4).unwrap() })),
        sample("entry_placeholder", entry(8, Body::ToolResult { call: CallId::new(4).unwrap(), outcome: Outcome::detached(), card: None, held: vec![protocol::Held::Stream(protocol::OutputStream { session: SESSION, call: CallId::new(4).unwrap() })] })),
        sample("entry_spawned_model_only", entry(1, Body::Spawned { intent: protocol::publication::SpawnImage { runner: protocol::publication::RunnerKind::ModelOnly, authority: protocol::Json::null(), image: protocol::publication::ContextImage::default() }.to_json().unwrap() })),
        sample("entry_model_thread", entry(2, Body::ModelCall { model: "example:model".into(), attempt: 0, choice: protocol::ToolChoice::None, r#for: CallId::new(1), purpose: Some("example".into()), request: Some(protocol::publication::ThreadImage { turn: protocol::Json::null(), authority: protocol::Json::null() }.to_json().unwrap()) })),
        sample("entry_spawned", entry(1, Body::Spawned { intent: protocol::publication::SpawnImage { runner: protocol::publication::RunnerKind::Agent, authority: protocol::Json::null(), image: context_image() }.to_json().unwrap() })),
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
                    watch: Seq::new(16).unwrap(),
                    outcome: Outcome::Done {
                        output: protocol::Json::encode(&json!({ "text": "found it" })).unwrap(),
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
                    image: None,
                    }],
                },
            ),
        ),
        sample(
            "entry_extensions_dynamic",
            entry(19, Body::Extensions { installed: vec![Installed {
                name: "counter".into(), version: 7,
                image: Some(protocol::Json::encode(&json!(["a.text{name='test',text='old'}", [], 4_194_304, 100_000, {"secs":0,"nanos":25_000_000}, true])).unwrap()),
            }] }),
        ),
        sample(
            "entry_ext",
            entry(
                20,
                Body::Ext {
                    ext: "shell".into(),
                    state: "cwd".into(),
                    data: protocol::Json::encode(&json!("/w")).unwrap(),
                },
            ),
        ),
        sample(
            "entry_context",
            entry(
                21,
                Body::Context {
                    publication: None,
                    text: "cwd: /w".into(),
                },
            ),
        ),
        sample(
            "entry_context_publication",
            entry(
                22,
                Body::Context {
                    text: "Instructions".into(),
                    publication: Some(protocol::publication::Publication {
                        namespace: "example".into(),
                        key: protocol::Json::encode("guide").unwrap(),
                    }),
                },
            ),
        ),
        sample("entry_interrupt", entry(25, Body::INTERRUPT)),
        sample(
            "delta",
            DaemonMsg::Delta {
                call: CallId::new(4).unwrap(),
                text: "hel".into(),
                attempt: 0,
            },
        ),
        sample(
            "activity",
            DaemonMsg::Activity {
                seq: 7,
                activity: Activity::Idle,
            },
        ),
        sample("fork_source_not_found", DaemonMsg::ForkRejected { reason: protocol::ForkError::SourceNotFound }),
        sample("fork_ambiguous_source", DaemonMsg::ForkRejected { reason: protocol::ForkError::AmbiguousSource }),
        sample("fork_invalid_sequence", DaemonMsg::ForkRejected { reason: protocol::ForkError::InvalidSequence }),
        sample("fork_unfinished", DaemonMsg::ForkRejected { reason: protocol::ForkError::Unfinished }),
        sample("fork_owned_resources", DaemonMsg::ForkRejected { reason: protocol::ForkError::OwnedResources }),
        sample("fork_disabled", DaemonMsg::ForkRejected { reason: protocol::ForkError::Disabled }),
        sample("fork_unavailable_generation", DaemonMsg::ForkRejected { reason: protocol::ForkError::UnavailableGeneration }),
        sample("fork_viewer", DaemonMsg::ForkRejected { reason: protocol::ForkError::Viewer }),
        sample("fork_shutting_down", DaemonMsg::ForkRejected { reason: protocol::ForkError::ShuttingDown }),
        sample("fork_storage", DaemonMsg::ForkRejected { reason: protocol::ForkError::Storage { message: "write failed".into() } }),
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
                    ReadOnly::Extensions,
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
            DaemonMsg::Snapshot(Box::new(SessionView::from_log(SESSION, "/w".into(), &log).snapshot())),
        ),
        sample(
            "snapshot_publications",
            DaemonMsg::Snapshot(Box::new(SessionView::from_log(
                SESSION, "/w".into(),
                ["skills", "mcp-schemas"].into_iter().zip(1..).map(|(namespace, seq)| Entry::new(
                    Seq::new(seq).unwrap(), Epoch(0), Body::Context {
                        text: "instructions or schema".into(),
                        publication: Some(protocol::publication::Publication {
                            namespace: namespace.into(), key: protocol::Json::encode(&"loaded-publication").unwrap(),
                        }),
                    },
                )),
            ).snapshot())),
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
        let read = protocol::Frame::from_line(entry.seq.get(), &line).unwrap();
        assert_eq!(read.as_bytes(), frame.as_bytes(), "{}", sample.name);
        assert_eq!((read.seq, read.class), (frame.seq, frame.class));
        assert_eq!(&Entry::from_json(frame.body()).unwrap(), entry);
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
        machine: None,
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
