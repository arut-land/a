//! The log schema's invariants: a line reads back as it was written, and a
//! log folds to what its session did.

use std::fs;
use std::path::{Path, PathBuf};

use protocol::ledger::{Call, Ledger, Phase};
use protocol::view::SessionView;
use protocol::{
    Activity, Author, Body, CallId, ClientId, ClientSeq, DaemonMsg, Decode, Entry, Frame, WaitOn,
};
use serde_json::Value;
use uuid::Uuid;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every entry line some build sent: the wire corpus's and the recorded
/// view vectors'.
fn recorded_lines() -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let corpus = root().join(format!("fixtures/wire/v{}/daemon", protocol::PROTO));
    for file in fs::read_dir(corpus).unwrap() {
        let bytes = fs::read(file.unwrap().path()).unwrap();
        let msg: Value = serde_json::from_slice(&bytes).unwrap();
        if msg["t"] == "entry" {
            lines.push(line_of(&bytes));
        }
    }
    let vectors: Vec<Value> = serde_json::from_slice(
        &fs::read(root().join("crates/protocol/tests/vectors/view.json")).unwrap(),
    )
    .unwrap();
    for vector in vectors {
        for msg in vector["messages"].as_array().unwrap() {
            // A vector's kinds from a newer build are skipped, not read.
            if msg["t"] == "entry" && Body::knows(msg["kind"].as_str().unwrap_or_default()) {
                lines.push(line_of(&serde_json::to_vec(msg).unwrap()));
            }
        }
    }
    lines
}

/// The log line an entry frame's body holds: the message without its tag.
fn line_of(frame: &[u8]) -> Vec<u8> {
    let frame = frame.strip_suffix(b"\n").unwrap_or(frame);
    let tagged = br#"{"t":"entry","#;
    if let Some(fields) = frame.strip_prefix(tagged) {
        return [b"{".as_slice(), fields].concat();
    }
    // A vector's message, its keys in another order: as an entry encodes.
    let entry = Entry::from_json(frame).unwrap();
    let mut line = Vec::new();
    entry.encode(&mut line).unwrap();
    line
}

/// A log's line decodes to its entry, which encodes to the very line
/// again: a replay sends what live clients were sent.
#[test]
fn a_log_replays_byte_identically() {
    let lines = recorded_lines();
    assert!(lines.len() > 30, "{}", lines.len());
    for line in [&lines[..], &log()[..]].concat() {
        let entry = Entry::from_json(&line).unwrap();
        let mut again = Vec::new();
        entry.encode(&mut again).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&again),
            String::from_utf8_lossy(&line)
        );
        let frame = Frame::from_line(entry.seq, &line).unwrap();
        assert_eq!(frame.as_bytes(), Frame::entry(&entry).unwrap().as_bytes());
        let DaemonMsg::Entry(read) = DaemonMsg::from_json(&frame.as_bytes()[4..]).unwrap() else {
            panic!("{}", String::from_utf8_lossy(&line));
        };
        assert_eq!(read, entry);
    }
}

/// Every recorded line folds, and what waits in the inbox is an open
/// tool call.
#[test]
fn every_recorded_log_folds() {
    let mut ledger = Ledger::default();
    for line in recorded_lines() {
        ledger.apply(&Entry::from_json(&line).unwrap());
    }
    assert!(
        ledger
            .inbox()
            .all(|c| matches!(ledger.call(c).map(|o| &o.call), Some(Call::Tool { .. })))
    );
}

/// The bytes of a payload (a tool's input, a model's reply, an extension's
/// state, an answer) are kept as they came, never parsed into a tree and
/// written again: odd spacing and key order survive the round trip.
#[test]
fn a_payload_is_never_parsed_on_the_wire_path() {
    let lines: [&[u8]; 5] = [
        br#"{"seq":2,"epoch":0,"kind":"tool_call","tool":"bash","input":{"z": 1,  "a":[ 2 ,3]}}"#,
        br#"{"seq":3,"epoch":0,"kind":"model_result","call":2,"outcome":{"status":"done","output":{ "message" :{"content":[]} }}}"#,
        br#"{"seq":4,"epoch":0,"kind":"ext","ext":"shell","state":"cwd","data":  "/w" }"#,
        br#"{"seq":5,"epoch":0,"kind":"answer","call":2,"value":{"b":true,  "a":1}}"#,
        br#"{"seq":6,"epoch":0,"kind":"decision","by":"h","call":2,"d":"pre_tool","input":[ 1,2 ]}"#,
    ];
    let payloads = [
        r#"{"z": 1,  "a":[ 2 ,3]}"#,
        r#"{ "message" :{"content":[]} }"#,
        r#""/w""#,
        r#"{"b":true,  "a":1}"#,
        r"[ 1,2 ]",
    ];
    for (line, payload) in lines.iter().zip(payloads) {
        let frame = Frame::from_line(1, line).unwrap();
        let DaemonMsg::Entry(entry) = DaemonMsg::from_json(&frame.as_bytes()[4..]).unwrap() else {
            panic!();
        };
        let encoded = Frame::entry(&entry).unwrap();
        let text = String::from_utf8_lossy(encoded.as_bytes());
        assert!(text.contains(payload), "{text} lost {payload}");
    }
}

/// A session as its log holds it: an `input` for each message, a gate's
/// decision after its call (an `ask` implies the wait), the user's answer,
/// a `take` naming the input it takes, a slash command and its settings.
fn log() -> Vec<Vec<u8>> {
    [
        r#"{"seq":1,"epoch":0,"kind":"context","text":"cwd: /w"}"#,
        r#"{"seq":2,"epoch":0,"kind":"input","text":"list files","taken":true}"#,
        r#"{"seq":3,"epoch":0,"kind":"model_call","model":"openai:m"}"#,
        r#"{"seq":4,"epoch":0,"kind":"model_result","call":3,"outcome":{"status":"done","output":{"text":"ok"}}}"#,
        r#"{"seq":5,"epoch":0,"kind":"tool_call","tool":"bash","input":{"command":"ls"}}"#,
        r#"{"seq":6,"epoch":0,"kind":"decision","by":"policy","call":5,"d":"permission","permission":"ask"}"#,
        r#"{"seq":7,"epoch":0,"kind":"input","text":"and sizes","mode":"steer"}"#,
        r#"{"seq":8,"epoch":0,"kind":"answer","call":5,"value":true}"#,
        r#"{"seq":9,"epoch":0,"kind":"tool_process","call":5,"pgid":42,"boot":"b"}"#,
        r#"{"seq":10,"epoch":0,"kind":"progress","call":5,"text":"src\n"}"#,
        r#"{"seq":11,"epoch":0,"kind":"take","input":7}"#,
        r#"{"seq":12,"epoch":0,"kind":"tool_result","call":5,"outcome":{"status":"done","output":{"exit":0}}}"#,
        r#"{"seq":13,"epoch":0,"kind":"model_call","model":"openai:m"}"#,
        r#"{"seq":14,"epoch":0,"kind":"model_result","call":13,"outcome":{"status":"done","output":{"text":"done"}}}"#,
        r#"{"seq":15,"epoch":0,"kind":"tool_call","tool":"model","input":{"args":"openai:n"},"by":"user"}"#,
        r#"{"seq":16,"epoch":0,"kind":"ext","ext":"settings","state":"model","data":"openai:n"}"#,
        r#"{"seq":17,"epoch":0,"kind":"tool_result","call":15,"outcome":{"status":"done","output":null}}"#,
        r#"{"seq":18,"epoch":0,"kind":"input","text":"later"}"#,
        r#"{"seq":19,"epoch":0,"kind":"tool_call","tool":"bash","input":{"command":"rm x"}}"#,
        r#"{"seq":20,"epoch":0,"kind":"decision","by":"policy","call":19,"d":"permission","permission":"ask"}"#,
    ]
    .map(|l| l.as_bytes().to_vec())
    .to_vec()
}

/// What a ledger says of a session, without the seqs.
#[derive(Debug, PartialEq)]
struct Said {
    open: Vec<(Option<String>, Phase, usize, Option<String>)>,
    commands: usize,
    inbox: Vec<Option<String>>,
    untaken: Vec<(String, bool)>,
    model: Option<String>,
    result: Option<String>,
    activity: Activity,
    context: bool,
}

fn said(ledger: &Ledger) -> Said {
    let input = |seq: u64| match ledger.call(seq).map(|o| &o.call) {
        Some(Call::Tool { input, .. }) => Some(input.raw().to_owned()),
        _ => None,
    };
    Said {
        open: ledger
            .open_calls()
            .map(|(seq, o)| {
                let phase = match &o.phase {
                    // How many decisions a gate made shows in its log only.
                    Phase::Gating { .. } => Phase::Gating { at: 0 },
                    phase => phase.clone(),
                };
                (input(seq), phase, o.processes.len(), o.progress.clone())
            })
            .collect(),
        commands: ledger.open_commands().count(),
        inbox: ledger.inbox().map(input).collect(),
        untaken: ledger
            .untaken()
            .iter()
            .map(|q| (q.text.clone(), q.steer))
            .collect(),
        model: ledger.settings().0.model,
        result: ledger.result().map(|r| format!("{r:?}")),
        activity: ledger.activity(),
        context: ledger.has_context(),
    }
}

/// The ledger of a log says where each of its calls and inputs stands,
/// at every point.
#[test]
fn a_log_folds_to_what_its_session_did() {
    let entries: Vec<Entry> = log().iter().map(|l| Entry::from_json(l).unwrap()).collect();
    let at = |seq| Ledger::of(entries.iter().take_while(|e| e.seq <= seq));
    let asked = said(&at(6));
    assert_eq!(asked.activity, Activity::Approval);
    assert_eq!(asked.inbox, [Some(r#"{"command":"ls"}"#.to_owned())]);
    let steered = said(&at(7));
    assert_eq!(steered.untaken, [("and sizes".to_owned(), true)]);
    let running = said(&at(10));
    assert_eq!(running.activity, Activity::Running);
    assert_eq!(running.inbox, [None; 0]);
    assert_eq!(running.open[0].1, Phase::Running);
    assert_eq!(
        (running.open[0].2, running.open[0].3.as_deref()),
        (1, Some("src\n"))
    );
    assert_eq!(said(&at(11)).untaken, []);
    let command = said(&at(15));
    assert_eq!((command.commands, command.open.len()), (1, 0));
    let end = Ledger::of(&entries);
    assert_eq!(end.activity(), Activity::Approval);
    assert_eq!(end.untaken().len(), 1);
    assert_eq!(end.settings().0.model.as_deref(), Some("openai:n"));
    assert_eq!(end.turn_start(), 2);
    assert!(end.has_context());
}

fn line(json: &str) -> Entry {
    Entry::from_json(json.as_bytes()).unwrap()
}

/// What waits is answered once: a second answer, from another client or a
/// resend, changes nothing.
#[test]
fn the_first_answer_wins() {
    let mut ledger = Ledger::default();
    for json in [
        r#"{"seq":1,"epoch":0,"kind":"tool_call","tool":"bash","input":{}}"#,
        r#"{"seq":2,"epoch":0,"kind":"wait","call":1,"on":"approval"}"#,
        r#"{"seq":3,"epoch":0,"kind":"answer","call":1,"value":false}"#,
        r#"{"seq":4,"epoch":0,"kind":"answer","call":1,"value":true}"#,
        r#"{"seq":5,"epoch":0,"kind":"wait","call":1,"on":"approval"}"#,
    ] {
        ledger.apply(&line(json));
    }
    assert_eq!(ledger.inbox().count(), 0);
    assert!(matches!(
        ledger.call(1).unwrap().phase,
        Phase::Settled(protocol::ledger::OutcomeKind::Failed)
    ));
}

/// Every wait is one lifecycle: a dialog, a client and a child wait as an
/// approval does, each answered by an `answer`, and the session's activity
/// says on what.
#[test]
fn every_wait_is_a_phase_of_its_call() {
    let mut ledger = Ledger::default();
    ledger.apply(&line(
        r#"{"seq":1,"epoch":0,"kind":"tool_call","tool":"pick_file","input":{}}"#,
    ));
    let cases = [
        (
            r#""on":"dialog","ui":{"title":"pick"}"#,
            WaitOn::Dialog {
                ui: protocol::Json::encode(&serde_json::json!({ "title": "pick" })).unwrap(),
            },
            Activity::Input,
        ),
        (
            r#""on":"client","tool":"pick_file""#,
            WaitOn::Client {
                tool: "pick_file".into(),
            },
            Activity::Client,
        ),
        (
            r#""on":"child","link":1"#,
            WaitOn::Child {
                link: protocol::Seq::new(1).unwrap(),
            },
            Activity::Running,
        ),
    ];
    for (seq, (fields, on, activity)) in (2..).step_by(2).zip(cases) {
        ledger.apply(&line(&format!(
            r#"{{"seq":{seq},"epoch":0,"kind":"wait","call":1,{fields}}}"#
        )));
        assert_eq!(ledger.call(1).unwrap().phase, Phase::Waiting(on));
        assert_eq!(ledger.activity(), activity);
        assert_eq!(ledger.inbox().count(), 0, "only approvals are asked");
        ledger.apply(&line(&format!(
            r#"{{"seq":{},"epoch":0,"kind":"answer","call":1,"value":"a.txt"}}"#,
            seq + 1
        )));
        assert_eq!(ledger.call(1).unwrap().phase, Phase::Running);
    }
}

/// A client's mutations carry who sent them, so the log alone says which
/// was the last of each client's it took.
#[test]
fn the_log_knows_each_clients_last_mutation() {
    let client = ClientId(Uuid::from_u128(0xc));
    let author = |seq| Author {
        client,
        seq: ClientSeq::new(seq).unwrap(),
    };
    let entries = [
        Body::Input {
            text: "hi".into(),
            mode: protocol::Mode::Queue,
            taken: true,
            origin: Some(author(1)),
        },
        Body::Interrupt {
            origin: Some(author(3)),
        },
        Body::Answer {
            call: CallId::new(9).unwrap(),
            value: protocol::Json::encode(&true).unwrap(),
            origin: Some(author(2)),
        },
    ];
    let mut ledger = Ledger::default();
    for (seq, body) in (1..).zip(entries) {
        ledger.apply(&Entry::new(seq, 0, body));
    }
    assert_eq!(ledger.last_of(client), ClientSeq::new(3));
    assert_eq!(ledger.last_of(ClientId(Uuid::from_u128(0xd))), None);
}

/// A retry's text replaces the failed attempt's in the view.
#[test]
fn a_retry_streams_one_attempt() {
    let mut view = SessionView::default();
    for (attempt, text) in [(0, "par"), (1, "whole")] {
        view.fold(&DaemonMsg::Delta {
            call: 1,
            text: text.into(),
            attempt,
        });
    }
    assert_eq!(view.streaming.unwrap().text, "whole");
}
