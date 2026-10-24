//! What the model reads of the queue: taken items where they were taken,
//! nothing withdrawn, and calls detached into the background.

use super::*;
use protocol::{Audience, Envelope, Epoch, Json, Mode, SessionId};

const SESSION: SessionId = SessionId::NIL;

fn transcript(bodies: Vec<Body>) -> Vec<Message> {
    let entries: Vec<_> = (bodies.into_iter().zip(1..))
        .map(|(body, seq)| Entry::new(Seq::new(seq).unwrap(), Epoch(0), body))
        .collect();
    let messages = Transcript::of(&entries, Arc::new(Registry::default()), "").into_messages();
    messages.iter().cloned().collect()
}

fn seq(n: u64) -> Seq {
    Seq::new(n).unwrap()
}

fn call(n: u64) -> CallId {
    CallId::new(n).unwrap()
}

fn input(text: &str, taken: bool) -> Body {
    Body::Input {
        segments: text.into(),
        mode: Mode::Steer,
        taken,
        origin: None,
        force: false,
    }
}

/// A reply calling `sleep` as `c0`, then the call, at seqs 2 and 3.
fn called() -> Vec<Body> {
    let reply = llm::Response {
        message: Assistant {
            content: vec![llm::AssistantPart::ToolCall(llm::ToolCall {
                id: "c0".into(),
                name: "sleep".into(),
                input: Json::null(),
                invalid: None,
                provider_executed: false,
                meta: llm::Meta::new(),
            })],
            origin: None,
        },
        finish: llm::Finish::new(llm::Reason::ToolCalls, None),
        usage: llm::Usage::default(),
        id: None,
        model: None,
        meta: llm::Meta::new(),
    };
    vec![
        input("work", true),
        Body::ModelResult {
            call: call(2),
            r#for: None,
            outcome: Outcome::Done {
                output: Json::encode(&reply).unwrap(),
            },
            failure: None,
        },
        Body::ToolCall {
            invoke: None,
            tool: "sleep".into(),
            input: Json::null(),
            id: Some("c0".into()),
            card: None,
            by: Audience::Model,
        },
    ]
}

fn texts(output: &llm::Output) -> Vec<String> {
    match output {
        llm::Output::Content(parts) => (parts.iter())
            .filter_map(|p| match p {
                llm::UserPart::Text(text) => Some(text.text.clone()),
                llm::UserPart::Media(_) => None,
            })
            .collect(),
        other => vec![text_of(other.clone())],
    }
}

#[test]
fn what_is_taken_into_a_result_is_read_with_it_and_never_between_a_call_and_its_result() {
    let mut bodies = called();
    bodies.extend([
        input("also this", false),
        Body::ToolResult {
            call: call(3),
            outcome: Outcome::Done {
                output: Json::encode("slept").unwrap(),
            },
            card: None,
            held: Vec::new(),
        },
        Body::Take {
            input: seq(4),
            into: Some(call(3)),
        },
    ]);
    let messages = transcript(bodies);
    let Some(Message::Tool(answers)) = messages.last() else {
        panic!("{messages:#?}");
    };
    assert_eq!(
        texts(&answers.results[0].output),
        [r#""slept""#, "<steer from=\"user\">\nalso this\n</steer>"]
    );
}

#[test]
fn a_withdrawn_input_is_never_read() {
    let messages = transcript(vec![
        input("work", true),
        input("never mind", false),
        Body::Withdraw {
            input: seq(2),
            origin: None,
        },
        Body::Take {
            input: seq(2),
            into: None,
        },
    ]);
    assert_eq!(messages, [Message::user("work")]);
}

#[test]
fn a_detached_call_answers_with_its_stream_and_its_outcome_is_read_once_taken() {
    let mut bodies = called();
    let outcome = Outcome::Done {
        output: Json::encode("slept").unwrap(),
    };
    bodies.extend([
        Body::Detached { call: call(3) },
        Body::ToolResult {
            call: call(3),
            outcome: Outcome::detached(),
            card: None,
            held: vec![Held::Stream(OutputStream {
                session: SESSION,
                call: call(3),
            })],
        },
        Body::Enqueue {
            envelope: Envelope::own(SESSION, call(3), EnvelopeId::DETACHED, outcome.payload()),
        },
    ]);
    let waiting = transcript(bodies.clone());
    let Some(Message::Tool(answers)) = waiting.last() else {
        panic!("{waiting:#?}");
    };
    let placeholder = texts(&answers.results[0].output).join("");
    assert!(placeholder.contains("stream:3"), "{placeholder}");
    bodies.push(Body::Take {
        input: seq(6),
        into: None,
    });
    let taken = transcript(bodies);
    let Some(Message::User(user)) = taken.last() else {
        panic!("{taken:#?}");
    };
    let text = format!("{user:?}");
    assert!(
        text.contains(r#"<background-result call=\"3\" command=\"sleep\" log=\"stream:3\">"#),
        "{text}"
    );
    assert!(text.contains("slept"), "{text}");
}

#[test]
fn a_detached_calls_stream_is_named_for_the_session_whose_log_holds_it() {
    let home = tempfile::tempdir().unwrap();
    let store = Arc::new(blob_files::Files::open(home.path().join("blobs"), home.path()).unwrap());
    let tree = Arc::new(blob_files::CacheDir::new(
        home.path().join("cache"),
        store.clone(),
    ));
    let session = |id| ext::blob::Blobs::new(store.clone(), tree.clone(), id);
    // As a child inherits it: the viewer is not the owner.
    let owner = OutputStream {
        session: SessionId::new(),
        call: call(3),
    };
    let stream = session(owner.session).stream(call(3), 1 << 20).unwrap();
    let mut bodies = called();
    bodies.extend([
        Body::Detached { call: call(3) },
        Body::ToolResult {
            call: call(3),
            outcome: Outcome::detached(),
            card: None,
            held: vec![Held::Stream(owner)],
        },
    ]);
    let entries: Vec<_> = (bodies.into_iter().zip(1..))
        .map(|(body, seq)| Entry::new(Seq::new(seq).unwrap(), Epoch(0), body))
        .collect();
    let places = session(SessionId::new()).places();
    let messages =
        Transcript::placed(&entries, Arc::new(Registry::default()), "", &places).into_messages();
    let Some(Message::Tool(answers)) = messages.iter().last() else {
        panic!("{messages:#?}");
    };
    let placeholder = texts(&answers.results[0].output).join("");
    let shown = stream.open.path().display().to_string();
    assert!(shown.contains(&owner.session.to_string()), "{shown}");
    assert!(placeholder.contains(&shown), "{placeholder}");
}

#[test]
fn a_held_blob_is_named_only_once_it_can_be_read() {
    let home = tempfile::tempdir().unwrap();
    let store = Arc::new(blob_files::Files::open(home.path().join("blobs"), home.path()).unwrap());
    let tree = Arc::new(blob_files::CacheDir::new(
        home.path().join("cache"),
        store.clone(),
    ));
    let places = ext::blob::Blobs::new(store.clone(), tree, SessionId::new()).places();
    let mut writer = ext::blob::BlobStore::writer(&*store, 1 << 20).unwrap();
    std::io::Write::write_all(&mut writer, b"whole").unwrap();
    let kept = writer
        .finish("text/plain", ext::blob::BlobOrigin::ToolOutput(call(3)))
        .unwrap();
    let mut gone = kept.clone();
    gone.id = ext::blob::BlobId([9; 32]);
    let shown = |blob: &ext::blob::BlobRef| {
        let mut bodies = called();
        bodies.push(Body::ToolResult {
            call: call(3),
            outcome: Outcome::Done {
                output: Json::encode("cut").unwrap(),
            },
            card: None,
            held: vec![Held::Blob(blob.clone())],
        });
        let entries: Vec<_> = (bodies.into_iter().zip(1..))
            .map(|(body, seq)| Entry::new(Seq::new(seq).unwrap(), Epoch(0), body))
            .collect();
        let messages = Transcript::placed(&entries, Arc::new(Registry::default()), "", &places)
            .into_messages();
        let Some(Message::Tool(answers)) = messages.iter().last() else {
            panic!("{messages:#?}");
        };
        texts(&answers.results[0].output).join("")
    };
    // As after a restart that lost the cache: naming it makes it again.
    let text = shown(&kept);
    let Some((_, path)) = (text.strip_suffix(']')).and_then(|t| t.rsplit_once("[full output: "))
    else {
        panic!("{text}")
    };
    let path = path.to_owned();
    assert_eq!(std::fs::read(&path).unwrap(), b"whole");
    std::fs::remove_dir_all(home.path().join("cache")).unwrap();
    assert!(shown(&kept).ends_with(&format!("[full output: {path}]")));
    assert_eq!(std::fs::read(&path).unwrap(), b"whole");
    // One the store does not have is not named at all.
    assert!(!shown(&gone).contains("full output"), "{}", shown(&gone));
}
