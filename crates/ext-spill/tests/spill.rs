//! Real tools through the rig, on its real blob store: what is recorded,
//! and what the model is then shown of it.

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};

use ext::host::Registry;
use ext::testing::Rig;
use ext::{Called, Cx, Extension, Placed, Registrar, Schema, Stored, Tool};
use ext_spill::Spill;
use protocol::{BlobOrigin, BlobRef, Held, Json, Outcome, Payload};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize, Schema)]
struct Lines {
    count: u32,
}

/// `count` numbered lines of 11 bytes each.
fn lines(count: u32) -> String {
    (0..count).fold(String::new(), |mut text, n| {
        let _ = writeln!(text, "line {n:05}");
        text
    })
}

/// Prints `count` numbered lines, as text.
struct Print;

impl Tool for Print {
    type In = Lines;
    type Out = String;

    const NAME: &'static str = "print";
    const DESCRIPTION: &'static str = "Prints numbered lines.";

    fn call(&self, input: &Lines, _: &Cx) -> Result<String, String> {
        Ok(lines(input.count))
    }

    fn to_model(output: &Json) -> llm::Output {
        llm::Output::Text(output.parse().unwrap())
    }
}

/// The same lines, shown to the model as the JSON it stored.
struct Dump;

impl Tool for Dump {
    type In = Lines;
    type Out = String;

    const NAME: &'static str = "dump";
    const DESCRIPTION: &'static str = "Dumps numbered lines.";

    fn call(&self, input: &Lines, cx: &Cx) -> Result<String, String> {
        Print.call(input, cx)
    }
}

/// Keeps its whole output in a blob of its own and shows a part, as the
/// shell does.
struct Keep;

#[derive(Serialize, Deserialize)]
struct Kept {
    shown: String,
    full: BlobRef,
}

impl Tool for Keep {
    type In = Lines;
    type Out = Kept;

    const NAME: &'static str = "keep";
    const DESCRIPTION: &'static str = "Keeps numbered lines.";

    fn call(&self, input: &Lines, cx: &Cx) -> Result<Kept, String> {
        let full = Print.call(input, cx)?;
        let mut writer = cx.blobs.writer(u64::MAX).map_err(|e| e.to_string())?;
        writer
            .write_all(full.as_bytes())
            .map_err(|e| e.to_string())?;
        let full = (writer.finish("text/plain", BlobOrigin::ToolOutput(cx.call)))
            .map_err(|e| e.to_string())?;
        Ok(Kept {
            shown: "line 00000\n".repeat(1000),
            full,
        })
    }

    fn to_model(output: &Json) -> llm::Output {
        llm::Output::Text(output.parse::<Kept>().unwrap().shown)
    }

    fn held(kept: &Kept) -> Vec<Held> {
        vec![Held::Blob(kept.full.clone())]
    }
}

struct Tools;

impl Extension for Tools {
    const NAME: &'static str = "tools";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Print).tool(Dump).tool(Keep);
    }
}

fn registry() -> Registry {
    Registry::default()
        .with(&Tools)
        .unwrap()
        .with(&Spill::default())
        .unwrap()
}

/// Where the engine would name what a result holds: naming is its
/// projection's (the engine's `transcript` tests), the cut is spill's.
const AT: &str = "tree/by-id/spilled";

/// What the model is shown of `called`: its payload read back from the
/// rig's store when a blob holds it.
fn shown(rig: &Rig, tool: &str, called: &Called) -> String {
    shown_at(rig, tool, called, Some(AT))
}

/// As [`shown`], its blob named at `at`, or nowhere.
fn shown_at(rig: &Rig, tool: &str, called: &Called, at: Option<&str>) -> String {
    let held: Vec<Placed> = (called.held.iter())
        .map(|held| Placed {
            held: held.clone(),
            at: Some(AT.to_owned()),
        })
        .collect();
    let stored = match &called.outcome {
        Outcome::Done {
            output: Payload::Blob(blob),
        } => Some(Stored {
            at: at.map(str::to_owned),
            text: Some(read(rig, blob)),
        }),
        _ => None,
    };
    match rig
        .registry
        .present_result(tool, &called.outcome, &held, stored)
        .output
    {
        llm::Output::Text(text) => text,
        llm::Output::Json(json) => json.raw().to_owned(),
        llm::Output::Content(_) => panic!("text"),
    }
}

/// The blob `called` is logged as, holding nothing else.
fn spill(called: &Called) -> &BlobRef {
    match (&called.outcome, &called.held[..]) {
        (
            Outcome::Done {
                output: Payload::Blob(blob),
            },
            [],
        ) => blob,
        other => panic!("one spill: {other:?}"),
    }
}

fn read(rig: &Rig, blob: &BlobRef) -> String {
    let mut bytes = String::new();
    rig.blobs
        .open(&blob.id)
        .unwrap()
        .read_to_string(&mut bytes)
        .unwrap();
    bytes
}

#[test]
fn a_result_over_the_limit_is_shown_as_its_ends_and_where_the_whole_is() {
    let rig = Rig::new("spill-over", registry());
    let called = rig.called("print", &json!({ "count": 1000 }));
    let whole = lines(1000);
    let blob = spill(&called);
    assert_eq!((read(&rig, blob), blob.size), (whole.clone(), 11_000));
    assert_eq!(blob.media_type, "text/plain");

    let text = shown(&rig, "print", &called);
    // 186 whole lines of 11 bytes each end: 2046 bytes.
    let (head, tail) = (&whole[..2046], &whole[whole.len() - 2046..]);
    let tag = format!(
        "<truncated bytes=\"{}\" path=\"{AT}\"/>",
        whole.len() - 2 * 2046
    );
    assert_eq!(text, format!("{head}{tag}\n{tail}"));
}

#[test]
fn a_json_result_is_spilled_as_the_json_the_model_reads() {
    let rig = Rig::new("spill-json", registry());
    let called = rig.called("dump", &json!({ "count": 1000 }));
    let blob = spill(&called);
    let output = Json::encode(&lines(1000)).unwrap();
    assert_eq!(read(&rig, blob), output.raw());
    assert_eq!(blob.media_type, "application/json");
    let text = shown(&rig, "dump", &called);
    assert!(text.starts_with("\"line 00000\\nline 00001"), "{text}");
    assert!(text.len() < output.raw().len() / 2, "{}", text.len());
}

#[test]
fn a_result_within_the_limit_is_shown_as_the_tool_shows_it() {
    let rig = Rig::new("spill-under", registry());
    // 372 lines: 4092 bytes.
    let called = rig.called("print", &json!({ "count": 372 }));
    assert_eq!(called.held, []);
    assert!(matches!(
        called.outcome,
        Outcome::Done {
            output: Payload::Inline(_)
        }
    ));
    let plain = Rig::new("spill-plain", Registry::default().with(&Tools).unwrap());
    let alone = plain.called("print", &json!({ "count": 372 }));
    assert_eq!(called.outcome, alone.outcome);
    assert_eq!(
        shown(&rig, "print", &called),
        shown(&plain, "print", &alone),
    );
}

/// Recovery reruns a safe call: the same spill, shown the same. (Across
/// a reload, with the cache gone: the engine's `spill` driver test.)
#[test]
fn a_call_run_again_records_the_same_spill() {
    let rig = Rig::new("spill-again", registry());
    let called = rig.called("print", &json!({ "count": 5000 }));
    let again = rig.called("print", &json!({ "count": 5000 }));
    assert_eq!(spill(&again).id, spill(&called).id);
    assert_eq!(shown(&rig, "print", &again), shown(&rig, "print", &called));
}

#[test]
fn a_result_that_holds_its_output_already_is_never_spilled_again() {
    let rig = Rig::new("spill-held", registry());
    let called = rig.called("keep", &json!({ "count": 1000 }));
    let (
        Outcome::Done {
            output: Payload::Inline(_),
        },
        [Held::Blob(full)],
    ) = (&called.outcome, &called.held[..])
    else {
        panic!("{called:?}");
    };
    assert_eq!(
        full.origin,
        BlobOrigin::ToolOutput(protocol::CallId::new(1).unwrap())
    );
    assert_eq!(shown(&rig, "keep", &called), "line 00000\n".repeat(1000));
}

/// Where the whole cannot be named, the cut is the same; only the tag says
/// nothing of where.
#[test]
fn a_spill_whose_file_cannot_be_named_is_cut_the_same() {
    let rig = Rig::new("spill-nowhere", registry());
    let called = rig.called("print", &json!({ "count": 1000 }));
    let named = shown(&rig, "print", &called);
    let nowhere = shown_at(&rig, "print", &called, None);
    assert_eq!(nowhere, named.replace(&format!(" path=\"{AT}\""), ""));
    assert!(nowhere.contains("<truncated bytes="), "{nowhere}");
}
