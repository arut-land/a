//! Hostile input for the wire: the framer, the JSON decoders and the view
//! reducer reject or skip what they cannot read, and never panic. A plain
//! test run feeds each target random input; `mise run fuzz` runs one
//! coverage-guided (cargo-bolero, libFuzzer).

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::LazyLock;

use bolero::TypeGenerator;
use protocol::view::SessionView;
use protocol::{ClientMsg, DaemonMsg, Decode, Entry, read_frame};
use serde_json::Value;

/// Any bytes, read as a stream of frames of each type both sides read.
#[test]
fn framer() {
    bolero::check!().for_each(|bytes: &[u8]| {
        frames::<DaemonMsg>(bytes);
        frames::<ClientMsg>(bytes);
        frames::<Entry>(bytes);
    });
}

fn frames<T: Decode>(bytes: &[u8]) {
    let mut stream = Cursor::new(bytes);
    while let Ok(Some(_)) = read_frame::<T>(&mut stream) {}
}

/// Any bytes as one message body; whatever decodes encodes again to
/// something that decodes.
#[test]
fn json_decode() {
    bolero::check!().for_each(|bytes: &[u8]| {
        if let Ok(msg) = DaemonMsg::from_json(bytes)
            && !matches!(msg, DaemonMsg::Unknown { .. })
        {
            again(&msg);
        }
        if let Ok(msg) = ClientMsg::from_json(bytes) {
            again(&msg);
        }
        if let Ok(entry) = Entry::from_json(bytes) {
            assert_eq!(again(&entry), entry);
        }
    });
}

fn again<T: Decode + serde::Serialize>(msg: &T) -> T {
    let bytes = serde_json::to_vec(msg).unwrap();
    T::from_json(&bytes).unwrap_or_else(|e| panic!("{}: {e}", String::from_utf8_lossy(&bytes)))
}

/// One message for the reducer: a real one from the shared vectors, its
/// sequence number, call, text or tag replaced, so sessions arrive out of
/// order, with gaps, repeats, calls that were never made and kinds this
/// build does not know.
#[derive(Clone, Debug, TypeGenerator)]
struct Op {
    template: u16,
    seq: Option<u8>,
    call: Option<u8>,
    text: Option<String>,
    kind: Option<u8>,
}

/// Every message of `vectors/view.json`.
static TEMPLATES: LazyLock<Vec<Value>> = LazyLock::new(|| {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/view.json");
    let vectors: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    vectors
        .iter()
        .flat_map(|v| v["messages"].as_array().unwrap().iter().cloned())
        .collect()
});

const KINDS: &[&str] = &[
    "input",
    "take",
    "tool_call",
    "tool_result",
    "model_call",
    "model_result",
    "decision",
    "wait",
    "answer",
    "fired",
    "from_the_future",
];

impl Op {
    /// The message's JSON, and what this build decodes it as.
    fn message(&self) -> Option<(Vec<u8>, DaemonMsg)> {
        let mut msg = TEMPLATES[usize::from(self.template) % TEMPLATES.len()].clone();
        let fields = msg.as_object_mut()?;
        if let Some(seq) = self.seq {
            fields.insert("seq".into(), seq.into());
        }
        if let Some(call) = self.call {
            fields.insert("call".into(), call.into());
        }
        if let Some(text) = &self.text {
            fields.insert("text".into(), text.as_str().into());
        }
        if let Some(kind) = self.kind {
            fields.insert("kind".into(), KINDS[usize::from(kind) % KINDS.len()].into());
        }
        let json = serde_json::to_vec(&msg).ok()?;
        let decoded = DaemonMsg::from_json(&json).ok()?;
        Some((json, decoded))
    }
}

/// Any sequence of messages folds without a panic, and the same through
/// the framer as given directly.
#[test]
fn view_reducer() {
    bolero::check!()
        .with_type::<Vec<Op>>()
        .for_each(|ops: &Vec<Op>| {
            let msgs: Vec<_> = ops.iter().filter_map(Op::message).collect();
            let mut direct = SessionView::default();
            let mut wire = Vec::new();
            for (json, msg) in &msgs {
                direct.fold(msg);
                // As a daemon sends it, unknown kinds included.
                wire.extend(u32::try_from(json.len() + 1).unwrap().to_le_bytes());
                wire.push(protocol::FrameKind::Json as u8);
                wire.extend(json);
            }
            let mut framed = SessionView::default();
            let mut stream = Cursor::new(wire);
            while let Some(msg) = read_frame::<DaemonMsg>(&mut stream).unwrap() {
                framed.fold(&msg);
            }
            assert_eq!(direct.state_hash(), framed.state_hash());
            assert_eq!(direct.seq, framed.seq);
        });
}
