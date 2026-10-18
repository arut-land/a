//! `rig-oracle <fixtures/rig>`: for every completion exchange in
//! `cassettes/**.yaml` whose reply is a stream, decode the reply with rig
//! 0.44 and write what rig made of it to `expected/<same path>.json`.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use bytes::Bytes;
use futures_lite::StreamExt;
use rig_core::completion::{AssistantContent, CompletionRequest, Message};
use rig_core::providers::{anthropic, gemini, openai};
use rig_core::streaming::Item;
use rig_http::http_client::{
    Error, HttpClientExt, LazyBody, MultipartForm, Request, Response, Result, StreamingResponse,
};
use rig_http::wasm_compat::{WasmCompatSend, WasmCompatSendStream};
use serde_json::{Value, json};

#[derive(Clone)]
struct Recorded {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn response<B>(&self, body: B) -> Response<B> {
        let mut builder = http::Response::builder().status(self.status);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        builder.body(body).expect("recorded response")
    }
}

impl HttpClientExt for Recorded {
    fn send<T, U>(
        &self,
        _: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let this = self.clone();
        async move {
            if !(200..300).contains(&this.status) {
                return Err(Error::non_success_with_details(
                    http::StatusCode::from_u16(this.status).expect("status"),
                    this.response(()).headers().clone(),
                    this.body.clone(),
                ));
            }
            let body = Bytes::from(this.body.clone());
            let lazy: LazyBody<U> = Box::pin(std::future::ready(Ok(U::from(body))));
            Ok(this.response(lazy))
        }
    }

    fn send_multipart<U>(
        &self,
        _: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        std::future::ready(Err(Error::instance(std::io::Error::other("multipart"))))
    }

    fn send_streaming<T>(
        &self,
        _: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let this = self.clone();
        async move {
            if !(200..300).contains(&this.status) {
                return Err(Error::non_success_with_details(
                    http::StatusCode::from_u16(this.status).expect("status"),
                    this.response(()).headers().clone(),
                    this.body.clone(),
                ));
            }
            // Small chunks, so frames split across reads as on a network.
            let chunks: Vec<Result<Bytes>> = this
                .body
                .as_bytes()
                .chunks(97)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect();
            let body: Pin<Box<dyn WasmCompatSendStream<InnerItem = Result<Bytes>>>> =
                Box::pin(futures_lite::stream::iter(chunks));
            Ok(this.response(body))
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("usage: rig-oracle <fixtures/rig>"));
    let cassettes = root.join("cassettes");
    let mut files = Vec::new();
    walk(&cassettes, &mut files);
    files.sort();
    let mut written = 0;
    for file in files {
        let rel = file.strip_prefix(&cassettes).expect("under cassettes");
        let provider = rel.iter().next().and_then(|p| p.to_str()).expect("provider dir");
        let text = std::fs::read_to_string(&file).expect("read cassette");
        let mut results = Vec::new();
        for (index, doc) in serde_yaml_ng::Deserializer::from_str(&text).enumerate() {
            let doc: serde_yaml_ng::Value = serde::Deserialize::deserialize(doc).expect("yaml");
            if let Some(result) = exchange(provider, &doc) {
                results.push(json!({"exchange": index, "result": result}));
            }
        }
        if results.is_empty() {
            continue;
        }
        let out = root.join("expected").join(rel).with_extension("json");
        std::fs::create_dir_all(out.parent().expect("parent")).expect("mkdir");
        let mut json = serde_json::to_string_pretty(&results).expect("json");
        json.push('\n');
        std::fs::write(&out, json).expect("write expected");
        written += 1;
    }
    eprintln!("wrote {written} expected files");
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_some_and(|e| e == "yaml") {
            out.push(path);
        }
    }
}

fn str_of<'a>(value: &'a serde_yaml_ng::Value, path: &[&str]) -> Option<&'a str> {
    let mut value = value;
    for key in path {
        value = value.get(*key)?;
    }
    value.as_str()
}

fn headers_of(value: &serde_yaml_ng::Value) -> Vec<(String, String)> {
    value
        .get("then")
        .and_then(|then| then.get("header"))
        .and_then(|h| h.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|h| Some((h.get("name")?.as_str()?.to_owned(), h.get("value")?.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// What rig makes of one exchange, or `None` when it is no streamed
/// completion of a wire this oracle covers.
fn exchange(provider: &str, doc: &serde_yaml_ng::Value) -> Option<Value> {
    let path = str_of(doc, &["when", "path"])?;
    let headers = headers_of(doc);
    let streamed = headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("content-type") && value.contains("event-stream"));
    let status = doc.get("then")?.get("status")?.as_u64()? as u16;
    if !streamed && (200..300).contains(&status) {
        return None;
    }
    let recorded = Recorded {
        status,
        headers,
        body: str_of(doc, &["then", "body"]).unwrap_or_default().to_owned(),
    };
    let sent: Value = str_of(doc, &["when", "body"])
        .and_then(|body| serde_json::from_str(body).ok())
        .unwrap_or_default();
    let model = sent
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| path.split("models/").nth(1)?.split(':').next())
        .unwrap_or("recorded-model")
        .to_owned();
    let model = model.as_str();
    let request = || {
        let mut request = CompletionRequest::from(vec![Message::user("hi")]);
        request.max_tokens = Some(1024);
        request
    };
    let outcome = if path.ends_with("/responses") {
        let dialect = openai_dialect(provider)?;
        let model = openai::OpenAIConfig::with_key(dialect, "k").connect(recorded).responses(model);
        run(model.stream(request()))
    } else if path.ends_with("/chat/completions") {
        let dialect = openai_dialect(provider)?;
        let model = openai::OpenAIConfig::with_key(dialect, "k").connect(recorded).chat(model);
        run(model.stream(request()))
    } else if path.ends_with("/messages") {
        let model = anthropic::AnthropicConfig::new("k").connect(recorded).completion(model);
        run(model.stream(request()))
    } else if path.contains(":streamGenerateContent") {
        let model = gemini::GeminiConfig::new("k").connect(recorded).completion(model);
        run(model.stream(request()))
    } else {
        return None;
    };
    Some(outcome)
}

fn openai_dialect(provider: &str) -> Option<&'static openai::wire::Dialect> {
    match provider {
        "xai" => Some(&rig_core::providers::xai::DIALECT),
        name => openai::wire::by_name(name),
    }
}

fn run(
    opened: std::result::Result<
        rig_core::streaming::Streamed<rig_core::operation::Completion>,
        rig_core::ProviderError,
    >,
) -> Value {
    let mut stream = match opened {
        Ok(stream) => stream,
        Err(error) => return json!({"error": error.to_string()}),
    };
    let mut events = 0usize;
    let mut failed = None;
    futures_lite::future::block_on(async {
        while let Some(item) = stream.next().await {
            match item {
                Ok(Item::Event(_)) => events += 1,
                Ok(_) => {}
                Err(error) => {
                    failed = Some(error.to_string());
                    break;
                }
            }
        }
    });
    if let Some(error) = failed {
        return json!({"error": error, "events": events});
    }
    match futures_lite::future::block_on(stream.finish()) {
        Ok(response) => {
            let parts: Vec<Value> = response.choice.iter().map(part).collect();
            let usage = &response.usage;
            json!({
                "parts": parts,
                "finish": response.finish_reason().map(|reason| format!("{reason:?}")),
                "refusal": response.error,
                "usage": {
                    "input": usage.input_tokens,
                    "output": usage.output_tokens,
                    "cache_read": usage.cached_input_tokens,
                    "cache_write": usage.cache_creation_input_tokens,
                    "reasoning": usage.reasoning_tokens,
                },
            })
        }
        Err(error) => json!({"error": error.to_string(), "events": events}),
    }
}

fn part(content: &AssistantContent) -> Value {
    match content {
        AssistantContent::Text(text) => json!({"type": "text", "text": text.text}),
        AssistantContent::Reasoning(reasoning) => {
            json!({"type": "reasoning", "text": reasoning.text, "redacted": reasoning.redacted})
        }
        AssistantContent::ToolCall(call) => json!({
            "type": "tool_call",
            "id": call.id.wire(),
            "name": call.function.name.as_str(),
            "input": call.function.arguments,
            "invalid": call.function.invalid_arguments,
        }),
        AssistantContent::Image(_) => json!({"type": "image"}),
        AssistantContent::Opaque(opaque) => json!({"type": "opaque", "kind": opaque.kind(), "replay": opaque.replay}),
        _ => json!({"type": "unknown"}),
    }
}
