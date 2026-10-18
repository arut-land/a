//! The decoder's hot path on recorded streams: the time it takes per frame,
//! the fastest of several rounds, printed (`--no-capture` shows it;
//! `--release` makes it meaningful).

use std::time::{Duration, Instant};

use super::provider;
use crate::protocol::{Flow, Frame, Target};
use crate::spec::Request;

/// The frames of every Chat Completions exchange in a cassette.
fn exchanges(cassette: &str) -> Vec<(String, Vec<String>)> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/rig/cassettes/");
    let text = std::fs::read_to_string(format!("{path}{cassette}")).expect("cassette");
    serde_yaml_ng::Deserializer::from_str(&text)
        .filter_map(|doc| {
            let doc: serde_yaml_ng::Value = serde::Deserialize::deserialize(doc).ok()?;
            let when = doc.get("when")?;
            if !when.get("path")?.as_str()?.ends_with("/chat/completions") {
                return None;
            }
            let sent: serde_json::Value = serde_json::from_str(when.get("body")?.as_str()?).ok()?;
            let body = doc.get("then")?.get("body")?.as_str()?;
            let frames = body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(str::to_owned)
                .collect();
            Some((sent["model"].as_str()?.to_owned(), frames))
        })
        .collect()
}

#[test]
fn the_hot_path_time_per_frame() {
    let (rounds, runs) = if cfg!(debug_assertions) {
        (1, 1)
    } else {
        (7, 100)
    };
    for (host, cassette) in [
        (
            "openai",
            "openai/streaming_grammar_chat/long_text_stream.yaml",
        ),
        ("venice", "venice/streaming/streaming_smoke.yaml"),
        (
            "deepseek",
            "deepseek/agent_tool_sessions/sequential_complex_tool_calls_streaming.yaml",
        ),
    ] {
        let provider = provider(host);
        let exchanges = exchanges(cassette);
        let frames: usize = exchanges.iter().map(|(_, frames)| frames.len()).sum();
        let mut events = Vec::with_capacity(4096);
        let mut fastest = Duration::MAX;
        for _ in 0..rounds {
            let start = Instant::now();
            for _ in 0..runs {
                for (model, frames) in &exchanges {
                    let target = Target {
                        provider: &provider,
                        model,
                    };
                    let mut decoder = provider.protocol.decoder(&target, &Request::default());
                    events.clear();
                    for data in frames {
                        let frame = Frame {
                            event: "message",
                            data,
                        };
                        if decoder.frame(frame, &mut events).expect("decodes") == Flow::Done {
                            break;
                        }
                    }
                }
            }
            fastest = fastest.min(start.elapsed());
        }
        let per_frame = fastest / (u32::try_from(frames).expect("frames") * runs);
        eprintln!("{cassette}: {frames} frames, {per_frame:?} per frame");
    }
}
