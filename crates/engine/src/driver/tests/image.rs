//! An image a tool's result holds, through the daemon: logged as a
//! reference beside the result, read back as bytes when the transcript is
//! made, and sent as base64 only in the request, alike after a reload.

use protocol::{BlobOrigin, BlobRef, Held};

use super::*;

/// A PNG's signature and header, enough to stand for one.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUg==";

/// The cassette's tool, answering with an image.
struct Shot;

#[derive(Serialize, serde::Deserialize)]
struct Taken {
    #[serde(skip)]
    blob: Option<BlobRef>,
}

impl Tool for Shot {
    type In = Nothing;
    type Out = Taken;

    const NAME: &'static str = "lookup_harbor_label";
    const DESCRIPTION: &'static str = "Return the alpha signal marker.";
    const REPLAY: Replay = Replay::Safe;

    fn call(&self, _: &Nothing, cx: &Cx) -> Result<Taken, String> {
        let mut writer = cx.blobs.writer(PNG.len() as u64).unwrap();
        writer.write_all(PNG).unwrap();
        let blob = writer
            .finish("image/png", BlobOrigin::ToolOutput(cx.call))
            .unwrap();
        Ok(Taken { blob: Some(blob) })
    }

    fn to_model(_: &Json) -> llm::Output {
        llm::Output::Text("the marker".into())
    }

    fn held(taken: &Taken) -> Vec<Held> {
        taken.blob.iter().cloned().map(Held::Blob).collect()
    }
}

struct Shots;

impl Extension for Shots {
    const NAME: &'static str = "shots";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Shot);
    }
}

/// What the request at `n` sent as the call's output.
fn sent_output(script: &Script, n: usize) -> Value {
    let sent = script.sent();
    let items = sent[n]["input"].as_array().unwrap();
    let result = (items.iter())
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    result["output"].clone()
}

#[test]
fn a_held_image_is_sent_as_base64_only_in_the_request_alike_after_reload() {
    let dir = tempfile::tempdir().unwrap();
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&[replies[0].clone(), replies[1].clone(), replies[1].clone()]);
    let store = Arc::new(crate::store::Jsonl::open(dir.path()).unwrap());
    let registry = Registry::default().with(&Shots).unwrap();
    let fixture = Fixture::on(store, ext::LOOP, registry, openai(&script));
    let header = Fixture::header();
    let session = Live::start(&header, &fixture.0, Open::Create, Owner::None).unwrap();
    session.submit("Call the tool, then answer.").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    let result = log
        .iter()
        .find(|e| matches!(e.body, Body::ToolResult { .. }));
    let result = result.expect("the call's result");
    let [Held::Blob(blob)] = &result.body.blobs()[..] else {
        panic!("{result:?}");
    };
    assert_eq!((blob.media_type.as_str(), blob.size), ("image/png", 16));
    let lines = serde_json::to_string(&log).unwrap();
    assert!(
        !lines.contains(PNG_BASE64),
        "the log holds the reference only"
    );

    let output = sent_output(&script, 1);
    let parts = output.as_array().expect("text and the image");
    assert_eq!(parts[0]["type"], "input_text");
    assert!(
        parts[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("the marker\n[image: "),
        "{output}"
    );
    assert_eq!(
        parts[1],
        json!({"type": "input_image", "detail": "auto",
            "image_url": format!("data:image/png;base64,{PNG_BASE64}")})
    );

    // Its transcript made again from the log, the cache gone: the same.
    std::fs::remove_dir_all(fixture.3.path().join("cache")).unwrap();
    let loaded = Live::start(&header, &fixture.0, Open::Load, Owner::None).unwrap();
    settle(&loaded);
    loaded.submit("And again.").unwrap();
    settle(&loaded);
    loaded.halt();
    assert_eq!(sent_output(&script, 2), output);
}
