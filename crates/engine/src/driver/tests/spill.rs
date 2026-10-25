//! A long tool result spilled, through the daemon: logged as the blob
//! that holds it, and shown the same after a reload that lost the blob
//! cache.

use std::fmt::Write as _;

use ext::point::PostTool;
use protocol::{BlobOrigin, Payload};

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

/// A run of the cassette's call of [`LongLabel`] on a registry, against
/// a real store, then of another turn after a reload with the blob cache
/// gone.
struct Spilled {
    log: Vec<Entry>,
    /// What each request sent as the result.
    shown: String,
    again: String,
    /// What the file the result names held, after the reload.
    whole: Option<String>,
}

fn spilled(registry: Registry) -> Spilled {
    let dir = tempfile::tempdir().unwrap();
    let replies = cassette("openai/streaming_grammar/tool_then_followup_text.yaml");
    let script = Script::new(&[replies[0].clone(), replies[1].clone(), replies[1].clone()]);
    let store = Arc::new(crate::store::Jsonl::open(dir.path()).unwrap());
    let fixture = Fixture::on(store, ext::LOOP, registry, openai(&script));
    let header = Fixture::header();
    let session = Live::start(&header, &fixture.0, Open::Create, Owner::None).unwrap();
    session.submit("Call the tool, then answer.").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();

    // None was made for a result logged inline.
    let cache = fixture.3.path().join("cache");
    if cache.exists() {
        std::fs::remove_dir_all(cache).unwrap();
    }
    let loaded = Live::start(&header, &fixture.0, Open::Load, Owner::None).unwrap();
    settle(&loaded);
    assert_eq!(log_of(&loaded), log);
    loaded.submit("And again.").unwrap();
    settle(&loaded);
    loaded.halt();
    let shown = sent_result(&script, 1);
    let whole = (shown.split_once("path=\""))
        .map(|(_, path)| std::fs::read_to_string(path.split_once('"').unwrap().0).unwrap());
    Spilled {
        log,
        shown,
        again: sent_result(&script, 2),
        whole,
    }
}

fn spill_registry() -> Registry {
    (Registry::default().with(&Long))
        .and_then(|r| r.with(&ext_spill::Spill::default()))
        .unwrap()
}

#[test]
fn a_spilled_result_is_logged_as_its_blob_alone_and_shown_alike_after_reload() {
    let Spilled {
        log,
        shown,
        again,
        whole,
    } = spilled(spill_registry());
    let result = log.iter().find_map(|e| match &e.body {
        Body::ToolResult {
            call,
            outcome,
            held,
            ..
        } => Some((*call, outcome, &held[..])),
        _ => None,
    });
    let Some((
        call,
        Outcome::Done {
            output: Payload::Blob(blob),
        },
        [],
    )) = result
    else {
        panic!("{log:#?}");
    };
    assert_eq!(blob.origin, BlobOrigin::ToolOutput(call));
    assert_eq!(
        (blob.size, blob.media_type.as_str()),
        (11_000, "text/plain")
    );
    // The bytes are in the blob, and nowhere in the log.
    let lines = serde_json::to_string(&log).unwrap();
    assert!(!lines.contains("line 00500"), "{lines}");
    assert!(shown.len() < 4500, "{shown}");
    assert!(shown.starts_with("line 00000\n") && shown.ends_with("line 00999\n"));

    // Loaded again from its file, with the cache gone: named again, made
    // again, and the request that follows shows the result byte for byte.
    assert_eq!(again, shown);
    assert_eq!(whole, Some(long()));
}

/// Replaces a result held as a blob with a note naming the blob.
struct Rewrite;

impl Extension for Rewrite {
    const NAME: &'static str = "rewrite";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.on::<PostTool>(|_, done| match done.outcome {
            Outcome::Done {
                output: Payload::Blob(blob),
            } => ext::Rewrite::Replace(Outcome::Done {
                output: Json::encode(&format!("{} bytes", blob.size))
                    .unwrap()
                    .into(),
            }),
            _ => ext::Rewrite::Pass,
        });
    }
}

/// `post_tool` runs on the result as it is logged, its payload already
/// in its blob: what it logs instead is what the model sees, and the blob
/// is the original its decision names, never a second copy of either.
#[test]
fn a_post_tool_rewrite_of_a_spilled_result_and_its_blob_agree() {
    let Spilled {
        log, shown, again, ..
    } = spilled(spill_registry().with(&Rewrite).unwrap());
    let original = log.iter().find_map(|e| match &e.body {
        Body::Decision {
            d: Verdict::PostTool { original },
            ..
        } => Some(original),
        _ => None,
    });
    let Some(Outcome::Done {
        output: Payload::Blob(blob),
    }) = original
    else {
        panic!("{log:#?}");
    };
    assert_eq!(blob.size, 11_000);
    let logged = log.iter().find_map(|e| match &e.body {
        Body::ToolResult { outcome, .. } => Some(outcome),
        _ => None,
    });
    let note = Json::encode("11000 bytes").unwrap();
    assert_eq!(
        logged,
        Some(&Outcome::Done {
            output: note.into()
        })
    );
    assert_eq!(
        (shown.as_str(), again.as_str()),
        ("11000 bytes", "11000 bytes")
    );
}

#[test]
fn a_result_within_the_limit_is_logged_inline() {
    let registry = Registry::default()
        .with(&Long)
        .and_then(|r| {
            r.with(&ext_spill::Spill {
                limit: 20_000,
                ..ext_spill::Spill::default()
            })
        })
        .unwrap();
    let Spilled {
        log, shown, again, ..
    } = spilled(registry);
    let logged = log.iter().find_map(|e| match &e.body {
        Body::ToolResult { outcome, held, .. } => Some((outcome, held)),
        _ => None,
    });
    let output = Json::encode(&long()).unwrap();
    assert_eq!(
        logged,
        Some((
            &Outcome::Done {
                output: output.into()
            },
            &Vec::new()
        ))
    );
    assert_eq!(
        (shown.as_str(), again.as_str()),
        (long().as_str(), long().as_str())
    );
}

/// Calls [`LongLabel`] and answers how long its output was.
struct Relay;

impl Tool for Relay {
    type In = Nothing;
    type Out = usize;

    const NAME: &'static str = "relay";
    const DESCRIPTION: &'static str = "Measures the label.";
    const REPLAY: Replay = Replay::Safe;

    async fn call(&self, _: &Nothing, cx: &Cx) -> Result<usize, String> {
        let input = Json::encode(&json!({})).unwrap();
        match cx.invoke("label", LongLabel::NAME, &input).await.outcome {
            Outcome::Done {
                output: Payload::Inline(output),
            } => Ok(output.parse::<String>().map_err(|e| e.to_string())?.len()),
            other => Err(format!("{other:?}")),
        }
    }
}

struct Relayed;

impl Extension for Relayed {
    const NAME: &'static str = "relayed";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(LongLabel).tool(Relay);
    }
}

/// What a call another invoked returns is its invoker's to read: never
/// moved into a blob, however long.
#[test]
fn an_invoked_result_reaches_its_invoker_as_its_tool_gave_it() {
    let registry = (Registry::default().with(&Relayed))
        .and_then(|r| r.with(&ext_spill::Spill::default()))
        .unwrap();
    let fixture = Fixture::new(ext_fixtures::ECHO, registry, openai(&Script::new(&[])));
    let session = fixture.create();
    session.submit("call relay {}").unwrap();
    settle(&session);
    let log = log_of(&session);
    session.halt();
    let results: Vec<_> = (log.iter())
        .filter_map(|e| match &e.body {
            Body::ToolResult { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    let (label, measured) = (
        Json::encode(&long()).unwrap(),
        Json::encode(&11_000).unwrap(),
    );
    assert_eq!(
        results,
        [
            Outcome::Done {
                output: label.into()
            },
            Outcome::Done {
                output: measured.into()
            },
        ],
        "{log:#?}"
    );
}
