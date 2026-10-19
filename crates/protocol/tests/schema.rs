//! The v2 schema's invariants: old logs read forever and as they were
//! written, and fold to what a v2 log of the same session folds to.

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
    let corpus = root().join("fixtures/wire/v1/daemon");
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

/// A line of an old log decodes to the kind it was written as, so the
/// entry encodes to the very line again: a replay sends old clients what
/// they were sent, and a v2 build reads the same bytes through the shims.
#[test]
fn old_logs_replay_byte_identically() {
    let lines = recorded_lines();
    assert!(lines.len() > 30, "{}", lines.len());
    for line in [&lines[..], &old_log()[..]].concat() {
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

/// Every recorded log folds, each entry as its shim says it, and its shim
/// is a kind v2 writes.
#[test]
fn every_old_log_folds() {
    let lines = recorded_lines();
    let mut ledger = Ledger::default();
    for line in &lines {
        let entry = Entry::from_json(line).unwrap();
        if let Some(shim) = entry.body.shim() {
            assert!(
                matches!(shim.shim(), Some(std::borrow::Cow::Borrowed(_))),
                "{shim:?}"
            );
        }
        ledger.apply(&entry);
    }
    assert!(ledger.inbox().iter().all(|&c| ledger.asked(c).is_some()));
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

/// A session as builds before v2 log it: the gate's decision ahead of its
/// call, an `ask`, the user's approval as a hook decision, a `steer` that
/// repeats its text, a slash command (run as it arrived) and `settings`.
fn old_log() -> Vec<Vec<u8>> {
    [
        r#"{"seq":1,"epoch":0,"kind":"context","text":"cwd: /w"}"#,
        r#"{"seq":2,"epoch":0,"kind":"user","text":"list files"}"#,
        r#"{"seq":3,"epoch":0,"kind":"model_call","model":"openai:m"}"#,
        r#"{"seq":4,"epoch":0,"kind":"model_result","call":3,"outcome":{"status":"done","output":{"text":"ok"}}}"#,
        r#"{"seq":5,"epoch":0,"kind":"hook","hook":"policy","decision":{"permission":{"tool":"bash","permission":"ask"}}}"#,
        r#"{"seq":6,"epoch":0,"kind":"tool_call","tool":"bash","input":{"command":"ls"}}"#,
        r#"{"seq":7,"epoch":0,"kind":"ask","call":6}"#,
        r#"{"seq":8,"epoch":0,"kind":"queued","text":"and sizes","steer":true}"#,
        r#"{"seq":9,"epoch":0,"kind":"hook","hook":"user","decision":{"approval":{"tool":"bash","approved":true,"call":6}}}"#,
        r#"{"seq":10,"epoch":0,"kind":"tool_process","call":6,"pgid":42,"boot":"b"}"#,
        r#"{"seq":11,"epoch":0,"kind":"progress","call":6,"text":"src\n"}"#,
        r#"{"seq":12,"epoch":0,"kind":"steer","text":"and sizes","queued":8}"#,
        r#"{"seq":13,"epoch":0,"kind":"tool_result","call":6,"outcome":{"status":"done","output":{"exit":0}}}"#,
        r#"{"seq":14,"epoch":0,"kind":"model_call","model":"openai:m"}"#,
        r#"{"seq":15,"epoch":0,"kind":"model_result","call":14,"outcome":{"status":"done","output":{"text":"done"}}}"#,
        r#"{"seq":16,"epoch":0,"kind":"command","name":"model","args":"openai:n"}"#,
        r#"{"seq":17,"epoch":0,"kind":"settings","model":"openai:n"}"#,
        r#"{"seq":18,"epoch":0,"kind":"command_result","call":16,"outcome":{"status":"done","output":null}}"#,
        r#"{"seq":19,"epoch":0,"kind":"queued","text":"later"}"#,
        r#"{"seq":20,"epoch":0,"kind":"hook","hook":"policy","decision":{"permission":{"tool":"bash","permission":"ask"}}}"#,
        r#"{"seq":21,"epoch":0,"kind":"tool_call","tool":"bash","input":{"command":"rm x"}}"#,
        r#"{"seq":22,"epoch":0,"kind":"ask","call":21}"#,
    ]
    .map(|l| l.as_bytes().to_vec())
    .to_vec()
}

/// The same session as v2 logs it: one `input` for each, the decision
/// after its call (which implies the wait), the answer as an `answer`, a
/// `take` naming the input it takes.
fn new_log() -> Vec<Vec<u8>> {
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

/// What a ledger says of a session, without the seqs, which differ between
/// two logs of it.
#[derive(Debug, PartialEq)]
struct Said {
    open: Vec<(Option<String>, Phase, usize, Option<String>)>,
    commands: usize,
    inbox: Vec<Option<String>>,
    untaken: Vec<(String, bool)>,
    model: Option<String>,
    result: Option<String>,
    prompt: Option<String>,
    tools: Vec<String>,
    failed: u32,
    running: u32,
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
        inbox: ledger.inbox().iter().map(|&c| input(c)).collect(),
        untaken: ledger
            .untaken()
            .iter()
            .map(|q| (q.text.clone(), q.steer))
            .collect(),
        model: ledger.settings().0.model,
        result: ledger.result().map(|r| format!("{r:?}")),
        prompt: ledger.prompt().map(str::to_owned),
        tools: ledger.turn_tools().to_vec(),
        failed: ledger.turn_failed(),
        running: ledger.turn_running(),
        activity: ledger.activity(),
        context: ledger.has_context(),
    }
}

/// An old log and a new log of the same session fold to the same ledger,
/// at every point the two have reached alike.
#[test]
fn an_old_and_a_new_log_of_the_same_session_fold_to_the_same_ledger() {
    let entries = |lines: Vec<Vec<u8>>| -> Vec<Entry> {
        lines.iter().map(|l| Entry::from_json(l).unwrap()).collect()
    };
    let (old, new) = (entries(old_log()), entries(new_log()));
    // Where the two logs stand alike: (old seq, new seq).
    let alike = [
        (2, 2),
        (4, 4),
        (7, 6),
        (8, 7),
        (9, 8),
        (11, 10),
        (12, 11),
        (13, 12),
        (14, 13),
        (15, 14),
        (16, 15),
        (17, 16),
        (18, 17),
        (19, 18),
        (22, 20),
    ];
    for (o, n) in alike {
        let at = |entries: &[Entry], seq| Ledger::of(entries.iter().take_while(|e| e.seq <= seq));
        assert_eq!(said(&at(&old, o)), said(&at(&new, n)), "old {o}, new {n}");
    }
    let end = Ledger::of(&new);
    assert_eq!(end.activity(), Activity::Approval);
    assert_eq!(end.untaken().len(), 1);
    assert_eq!(end.settings().0.model.as_deref(), Some("openai:n"));
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
    assert_eq!(ledger.inbox(), [0; 0]);
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
                ui: serde_json::json!({ "title": "pick" }).into(),
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
        assert_eq!(ledger.inbox(), [0; 0], "only approvals are asked");
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
            value: true.into(),
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
#[ignore = "v2: wave 4e"]
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
