//! Every streamed completion rig recorded (`fixtures/rig/cassettes`) decodes
//! to what rig itself made of it (`fixtures/rig/expected`), wire by wire.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use llm::http::{BoxFuture, Http, Payload, Response, body};
use llm::protocols::{chat, gemini, messages, responses};
use llm::{AssistantPart, Kind, Provider, Reason, Request};
use serde_json::{Value, json};

struct Recorded {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Http for Recorded {
    fn send(&self, _: http::Request<Payload>) -> BoxFuture<'static, io::Result<Response>> {
        let mut builder = http::Response::builder().status(self.status);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        // Small chunks, so frames split across reads as on a network.
        let chunks: Vec<Vec<u8>> = self
            .body
            .as_bytes()
            .chunks(97)
            .map(<[u8]>::to_vec)
            .collect();
        let response = builder.body(body(chunks)).expect("recorded response");
        Box::pin(std::future::ready(Ok(response)))
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/rig")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("fixtures") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "json") {
            out.push(path);
        }
    }
}

/// The provider a recorded exchange went to, as this crate speaks to it.
fn provider(dir: &str, path: &str) -> Option<Provider> {
    let auth = llm::Auth::None;
    if path.ends_with("/responses") {
        let (host, quirks) = responses::HOSTS.iter().find(|(host, _)| host.id == dir)?;
        return Some(Provider::new(
            host.id,
            "http://recorded",
            auth,
            responses::Responses::new(*quirks),
        ));
    }
    if path.ends_with("/chat/completions") {
        let (host, quirks) = chat::HOSTS.iter().find(|(host, _)| host.id == dir)?;
        return Some(Provider::new(
            host.id,
            "http://recorded",
            auth,
            chat::Chat::new(*quirks),
        ));
    }
    if path.contains(":streamGenerateContent") {
        let (host, quirks) = gemini::HOSTS.iter().find(|(host, _)| host.id == dir)?;
        return Some(Provider::new(
            host.id,
            "http://recorded",
            auth,
            gemini::Gemini::new(*quirks),
        ));
    }
    // The oracle speaks Anthropic's own dialect to every Messages path, a
    // gateway's `/api/v1/messages` included.
    if path.ends_with("/messages") {
        let (host, quirks) = messages::HOSTS
            .iter()
            .find(|(host, _)| host.id == "anthropic")?;
        return Some(Provider::new(
            host.id,
            "http://recorded",
            auth,
            messages::Messages::new(*quirks),
        ));
    }
    None
}

fn yaml_str<'a>(value: &'a serde_yaml_ng::Value, path: &[&str]) -> Option<&'a str> {
    let mut value = value;
    for key in path {
        value = value.get(*key)?;
    }
    value.as_str()
}

/// Our reply to one exchange, in the oracle's terms.
fn decode(dir: &str, doc: &serde_yaml_ng::Value) -> Option<Value> {
    let path = yaml_str(doc, &["when", "path"])?;
    let provider = provider(dir, path)?;
    let sent: Value = yaml_str(doc, &["when", "body"])
        .and_then(|body| serde_json::from_str(body).ok())
        .unwrap_or_default();
    // Gemini names the model in the path: `/v1beta/models/{model}:…`.
    let model = sent
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| path.split("models/").nth(1)?.split(':').next())
        .unwrap_or("recorded-model");
    let headers = doc
        .get("then")?
        .get("header")
        .and_then(|header| header.as_sequence())
        .map_or_default(|seq| {
            seq.iter()
                .filter_map(|h| {
                    Some((
                        h.get("name")?.as_str()?.to_owned(),
                        h.get("value")?.as_str()?.to_owned(),
                    ))
                })
                .collect()
        });
    let recorded = Recorded {
        status: u16::try_from(doc.get("then")?.get("status")?.as_u64()?).ok()?,
        headers,
        body: yaml_str(doc, &["then", "body"])
            .unwrap_or_default()
            .to_owned(),
    };
    let request = Request {
        model: model.into(),
        messages: vec![llm::Message::user("hi")].into(),
        // What the oracle asked for.
        generation: llm::Generation {
            max_output_tokens: std::num::NonZeroU32::new(1024),
            ..llm::Generation::default()
        },
        ..Request::default()
    };
    let reply = rt::block_on(async {
        let mut stream = llm::stream(&recorded, &provider, &request);
        let (mut finishes, mut failure) = (0, None);
        while let Some(event) = stream.next().await {
            assert!(
                failure.is_none() && finishes == 0,
                "event after a terminal outcome"
            );
            match event {
                Ok(llm::Event::Finish { .. }) => finishes += 1,
                Ok(_) => assert_eq!(finishes, 0, "events after Finish"),
                Err(error) => {
                    assert!(failure.is_none(), "more than one terminal error");
                    failure = Some(error);
                }
            }
        }
        assert_eq!(finishes + usize::from(failure.is_some()), 1);
        assert!(
            stream.next().await.is_none(),
            "a terminal stream stays ended"
        );
        match failure {
            Some(error) => Err(error),
            None => stream.finish(Some(llm::origin(&provider, &request.model))),
        }
    });
    Some(match reply {
        Err(error) => json!({"error": error.to_string(), "kind": format!("{:?}", error.kind)}),
        Ok(response) => {
            let parts: Vec<Value> = response
                .message
                .content
                .iter()
                .map(|part| part_of(part, provider.protocol.key().as_str()))
                .collect();
            let finish = &response.finish;
            json!({
                "parts": parts,
                "finish": match finish.reason {
                    Reason::Stop => "Stop".to_owned(),
                    Reason::Length => "Length".to_owned(),
                    Reason::ToolCalls => "ToolCalls".to_owned(),
                    Reason::ContentFilter => "ContentFilter".to_owned(),
                    Reason::Error | Reason::Other => format!("Other({:?})", finish.raw.clone().unwrap_or_default()),
                },
                "refusal": finish.message,
                "usage": response.usage,
            })
        }
    })
}

fn part_of(part: &AssistantPart, key: &str) -> Value {
    match part {
        AssistantPart::Text(text) => json!({"type": "text", "text": text.text}),
        AssistantPart::Reasoning(reasoning) => {
            json!({"type": "reasoning", "text": reasoning.text, "redacted": reasoning.redacted})
        }
        AssistantPart::ToolCall(call) => json!({
            "type": "tool_call",
            "id": call.id,
            "name": call.name,
            "input": call.input,
            "invalid": call.invalid,
        }),
        AssistantPart::Media(_) => json!({"type": "image"}),
        AssistantPart::Opaque(opaque) => {
            let state = opaque.meta.get(key);
            json!({
                "type": "opaque",
                "kind": state.as_ref().and_then(|state| state.pointer("/item/type")),
                "replay": state.as_ref().and_then(|state| state.get("replay")).and_then(Value::as_bool).unwrap_or(false),
            })
        }
        AssistantPart::ToolResult(_) => json!({"type": "tool_result"}),
    }
}

/// The counts rig reports; the rest of ours are checked per protocol.
const RIG_COUNTS: [&str; 5] = ["input", "output", "cache_read", "cache_write", "reasoning"];

/// Whether `ours` says what rig's `expected` says.
fn agrees(expected: &Value, ours: &Value) -> bool {
    if expected.get("error").is_some() || ours.get("error").is_some() {
        return expected.get("error").is_some() == ours.get("error").is_some();
    }
    let usage = |value: &Value| -> BTreeMap<String, Value> {
        value["usage"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, count)| !count.is_null() && RIG_COUNTS.contains(&key.as_str()))
            .map(|(key, count)| (key.clone(), count.clone()))
            .collect()
    };
    let ids_agree = |expected: &Value, ours: &Value| {
        let (expected, ours) = (
            expected.as_str().unwrap_or_default(),
            ours.as_str().unwrap_or_default(),
        );
        // Where rig issued its own id, so must we.
        let issued = |id: &str| id.len() == 36 && id.matches('-').count() == 4;
        let ours_issued = ours.len() == 21 && ours.starts_with("call_");
        if issued(expected) {
            ours_issued
        } else {
            expected == ours
        }
    };
    let parts_agree = match (expected["parts"].as_array(), ours["parts"].as_array()) {
        (Some(expected), Some(ours)) if expected.len() == ours.len() => {
            expected.iter().zip(ours).all(|(e, o)| {
                let mut e = e.clone();
                let mut o = o.clone();
                let ids = ids_agree(&e["id"], &o["id"]);
                if let (Some(e), Some(o)) = (e.as_object_mut(), o.as_object_mut()) {
                    e.remove("id");
                    o.remove("id");
                }
                ids && e == o
            })
        }
        _ => false,
    };
    parts_agree
        && expected["finish"] == ours["finish"]
        && expected["refusal"] == ours["refusal"]
        && usage(expected) == usage(ours)
}

#[test]
fn terminal_stream_semantics_match_the_recorded_corpus() {
    let root = root();
    let mut files = Vec::new();
    walk(&root.join("expected"), &mut files);
    files.sort();
    let (mut checked, mut skipped) = (0, 0);
    let mut failures = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(root.join("expected"))
            .expect("under expected");
        let dir = rel
            .iter()
            .next()
            .and_then(|dir| dir.to_str())
            .expect("provider")
            .to_owned();
        let cassette = root.join("cassettes").join(rel).with_extension("yaml");
        let text = std::fs::read_to_string(&cassette).expect("cassette");
        let docs: Vec<serde_yaml_ng::Value> = serde_yaml_ng::Deserializer::from_str(&text)
            .map(|doc| serde::Deserialize::deserialize(doc).expect("yaml"))
            .collect();
        let expected: Vec<Value> =
            serde_json::from_str(&std::fs::read_to_string(&file).expect("expected")).expect("json");
        for entry in expected {
            let index =
                usize::try_from(entry["exchange"].as_u64().expect("exchange")).expect("index");
            let Some(ours) = decode(&dir, &docs[index]) else {
                skipped += 1;
                continue;
            };
            checked += 1;
            if !agrees(&entry["result"], &ours) {
                failures.push(format!(
                    "{} #{index}\n  rig:  {}\n  ours: {}",
                    rel.display(),
                    entry["result"],
                    ours
                ));
            }
        }
    }
    eprintln!("parity: {checked} exchanges checked, {skipped} on wires not ported yet");
    let _ = Kind::Decode;
    assert!(
        failures.is_empty(),
        "{} of {checked} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
