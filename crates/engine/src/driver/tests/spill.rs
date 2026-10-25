//! A long tool result spilled, through the daemon: logged with the blob it
//! holds, and shown the same after a reload that lost the blob cache.

use std::fmt::Write as _;

use protocol::{BlobOrigin, Held};

use super::*;

/// The cassette's tool, answering at length.
struct LongLabel;

impl Tool for LongLabel {
    type In = Nothing;
    type Out = String;

    const NAME: &'static str = "lookup_harbor_label";
    const DESCRIPTION: &'static str = "Return the alpha signal marker.";
    const REPLAY: Replay = Replay::Safe;

    fn call(&self, _: &Nothing, _: &Cx) -> Result<String, String> {
        Ok(long())
    }

    fn to_model(label: &Json) -> llm::Output {
        llm::Output::Text(label.parse().unwrap())
    }
}

/// 1000 numbered lines of 11 bytes each.
fn long() -> String {
    (0..1000).fold(String::new(), |mut text, n| {
        let _ = writeln!(text, "line {n:05}");
        text
    })
}

struct Long;

impl Extension for Long {
    const NAME: &'static str = "long";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(LongLabel);
    }
}

/// What the request at `n` sent the model as the tool's result.
fn sent_result(script: &Script, n: usize) -> String {
    let sent = script.sent();
    let items = sent[n]["input"].as_array().unwrap();
    let result = (items.iter())
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    result["output"].as_str().unwrap().to_owned()
}

#[test]
fn a_spilled_result_survives_commit_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&[replies[0].clone(), replies[1].clone(), replies[1].clone()]);
    let registry = (Registry::default().with(&Long))
        .and_then(|r| r.with(&ext_spill::Spill::default()))
        .unwrap();
    let store = Arc::new(crate::store::Jsonl::open(dir.path()).unwrap());
    let fixture = Fixture::on(store, ext::LOOP, registry, openai(&script));
    let header = Fixture::header();
    let session = Live::start(&header, &fixture.0, Open::Create, Owner::None).unwrap();
    session.submit("Call the tool, then answer.").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let held = log.iter().find_map(|e| match &e.body {
        Body::ToolResult { held, .. } => Some(held.clone()),
        _ => None,
    });
    let Some([Held::Blob(blob)]) = held.as_deref() else {
        panic!("{log:#?}");
    };
    assert!(matches!(blob.origin, BlobOrigin::Spilled(_)), "{blob:?}");
    let shown = sent_result(&script, 1);
    let (_, path) = shown.split_once("path=\"").unwrap();
    let (path, _) = path.split_once('"').unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), long());
    assert!(shown.len() < 4500, "{shown}");

    // Loaded again from its file, with the cache gone: named again, made
    // again, and the request that follows shows the result byte for byte.
    std::fs::remove_dir_all(fixture.3.path().join("cache")).unwrap();
    let loaded = Live::start(&header, &fixture.0, Open::Load, Owner::None).unwrap();
    settle(&loaded);
    assert_eq!(log_of(&loaded), log);
    loaded.submit("And again.").unwrap();
    settle(&loaded);
    loaded.halt();
    assert_eq!(sent_result(&script, 2), shown);
    assert_eq!(std::fs::read_to_string(path).unwrap(), long());
}
