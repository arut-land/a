//! The protocol subset shared by the two transports. It never retries
//! requests: a server may have effected a call before losing its reply.

use std::future::poll_fn;
use std::sync::atomic::{AtomicU64, Ordering};

use ext::Cx;
use llm::Http;
use serde_json::{Value, json};

use crate::Failure;

pub(crate) const LIMIT: usize = 1024 * 1024;
pub(crate) const VERSION: &str = "2026-07-28";

pub(crate) fn request(id: u64, method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name":"a","version":env!("CARGO_PKG_VERSION")},
        "progressToken": id
    });
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

pub(crate) fn decode(message: &Value, id: u64, cx: &Cx) -> Result<Option<Value>, Failure> {
    if message["jsonrpc"] != "2.0" || !message.is_object() {
        return Err(Failure::Protocol("expected a JSON-RPC 2.0 object".into()));
    }
    if message.get("id").is_none() {
        let method = message["method"]
            .as_str()
            .ok_or_else(|| Failure::Protocol("notification has no method".into()))?;
        if method == "notifications/progress" && message["params"]["progressToken"] == id {
            let params = &message["params"];
            if let Some(text) = params["message"].as_str() {
                cx.progress(text);
            }
        }
        return Ok(None);
    }
    if message["id"] != id
        || message.get("method").is_some()
        || message.get("result").is_some() == message.get("error").is_some()
    {
        return Err(Failure::Protocol("invalid or mismatched response".into()));
    }
    if let Some(error) = message.get("error") {
        return Err(Failure::Remote {
            code: error["code"]
                .as_i64()
                .ok_or_else(|| Failure::Protocol("error has no integer code".into()))?,
            message: error["message"]
                .as_str()
                .ok_or_else(|| Failure::Protocol("error has no message".into()))?
                .to_owned(),
        });
    }
    let result = message["result"].clone();
    match result["resultType"].as_str() {
        Some("complete") => Ok(Some(result)),
        Some("input_required") => Err(Failure::InputRequired),
        _ => Err(Failure::Protocol("unknown or missing resultType".into())),
    }
}

pub(crate) async fn http(
    url: &str,
    method: &str,
    params: Value,
    headers: http::HeaderMap,
    cx: &Cx,
) -> Result<Value, Failure> {
    let client = llm::Ureq::new(std::sync::Arc::new(CallRuntime(cx.clone())));
    exchange(&client, url, method, params, headers, cx).await
}

async fn exchange(
    client: &dyn Http,
    url: &str,
    method: &str,
    params: Value,
    headers: http::HeaderMap,
    cx: &Cx,
) -> Result<Value, Failure> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let message = request(id, method, params);
    let bytes = serde_json::to_vec(&message).map_err(protocol)?;
    if bytes.len() > LIMIT {
        return Err(Failure::Limit);
    }
    let mut builder = http::Request::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", VERSION)
        .header("mcp-method", method);
    if let Some(name) = message["params"]["name"].as_str() {
        builder = builder.header("mcp-name", crate::headers::value(name));
    }
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(llm::http::Payload::from(bytes))
        .map_err(protocol)?;
    let response = client.send(request).await.map_err(transport)?;
    let status = response.status();
    let kind = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let sse = kind.split(';').next() == Some("text/event-stream");
    if !sse && kind.split(';').next() != Some("application/json") {
        return Err(Failure::Protocol(format!(
            "HTTP {status}: unsupported content type"
        )));
    }
    let mut body = response.into_body();
    let mut pending = Vec::new();
    let mut event = Vec::new();
    let mut total = 0;
    while let Some(chunk) = poll_fn(|cx| body.as_mut().poll_next(cx)).await {
        let bytes = chunk.map_err(transport)?;
        total += bytes.len();
        if total > LIMIT {
            return Err(Failure::Limit);
        }
        pending.extend(bytes);
        if sse {
            while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                let line = pending[..end]
                    .strip_suffix(b"\r")
                    .unwrap_or(&pending[..end]);
                if line.is_empty() {
                    if !event.is_empty() {
                        let value = serde_json::from_slice(&event).map_err(protocol)?;
                        event.clear();
                        if let Some(result) = decode(&value, id, cx)? {
                            return http_result(status, result);
                        }
                    }
                } else if let Some(data) = line.strip_prefix(b"data:") {
                    if !event.is_empty() {
                        event.push(b'\n');
                    }
                    event.extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
                }
                pending.drain(..=end);
            }
        }
    }
    if sse {
        return Err(Failure::Protocol("SSE ended before its response".into()));
    }
    let value = serde_json::from_slice(&pending).map_err(protocol)?;
    let result = decode(&value, id, cx)?
        .ok_or_else(|| Failure::Protocol(format!("HTTP {status}: no response")))?;
    http_result(status, result)
}

fn http_result(status: http::StatusCode, result: Value) -> Result<Value, Failure> {
    if status.is_success() {
        Ok(result)
    } else {
        Err(Failure::Transport(format!("HTTP {status}")))
    }
}

// Retain the call only as long as its HTTP request needs the runtime.
struct CallRuntime(Cx);
impl rt::Runtime for CallRuntime {
    fn spawn(&self, task: rt::BoxFuture<'static, ()>) {
        self.0
            .service::<dyn rt::Runtime>()
            .expect("engine runtime")
            .spawn(task);
    }
    fn spawn_blocking(&self, work: Box<dyn FnOnce() + Send>) {
        self.0
            .service::<dyn rt::Runtime>()
            .expect("engine runtime")
            .spawn_blocking(work);
    }
    fn reactor(&self) -> rt::Reactor {
        self.0
            .service::<dyn rt::Runtime>()
            .expect("engine runtime")
            .reactor()
    }
}

fn protocol(error: impl std::fmt::Display) -> Failure {
    Failure::Protocol(error.to_string())
}
fn transport(error: impl std::fmt::Display) -> Failure {
    Failure::Transport(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ext::host::Registry;
    use ext::testing::Rig;

    struct Peer {
        sse: bool,
    }
    impl Http for Peer {
        fn send(
            &self,
            request: http::Request<llm::http::Payload>,
        ) -> llm::http::BoxFuture<'static, std::io::Result<llm::http::Response>> {
            assert_eq!(request.method(), "POST");
            assert_eq!(request.headers()["mcp-protocol-version"], VERSION);
            assert_eq!(request.headers()["mcp-method"], "tools/call");
            assert_eq!(request.headers()["mcp-name"], "echo");
            let request: Value = serde_json::from_slice(&request.body().to_vec()).unwrap();
            let id = &request["id"];
            let response = json!({"jsonrpc":"2.0","id":id,"result":{"resultType":"complete","content":[{"type":"text","text":"ok"}]}});
            let (kind, bytes) = if self.sse {
                let progress = json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":id,"message":"working"}});
                ("text/event-stream", format!(": heartbeat\r\ndata: {progress}\r\n\r\nevent: message\r\ndata: {response}\r\n\r\n").into_bytes())
            } else {
                ("application/json", serde_json::to_vec(&response).unwrap())
            };
            // Split every byte, including CRLF and UTF-8 boundaries.
            let body = llm::http::body(bytes.into_iter().map(|b| vec![b]));
            let response = http::Response::builder()
                .header("content-type", kind)
                .body(body)
                .unwrap();
            Box::pin(std::future::ready(Ok(response)))
        }
    }

    #[test]
    fn http_accepts_fragmented_json_and_request_scoped_sse() {
        for sse in [false, true] {
            let rig = Rig::new(
                if sse { "mcp-sse" } else { "mcp-json" },
                Registry::default(),
            );
            let cx = rig.cx();
            let result = rt::block_on(exchange(
                &Peer { sse },
                "http://fixture.invalid/mcp",
                "tools/call",
                json!({"name":"echo","arguments":{}}),
                http::HeaderMap::new(),
                &cx,
            ))
            .unwrap();
            assert_eq!(result["content"][0]["text"], "ok");
            assert_eq!(
                *rig.progress.lock().unwrap(),
                if sse { "working" } else { "" }
            );
        }
    }

    #[test]
    fn responses_must_match_one_pending_request_and_one_outcome() {
        let rig = Rig::new("mcp-rpc", Registry::default());
        let cx = rig.cx();
        for message in [
            json!({"jsonrpc":"1.0","id":1,"result":{"resultType":"complete"}}),
            json!({"jsonrpc":"2.0","id":2,"result":{"resultType":"complete"}}),
            json!({"jsonrpc":"2.0","id":1,"result":{"resultType":"complete"},"error":{"code":-1,"message":"error"}}),
            json!({"jsonrpc":"2.0","id":1,"result":{}}),
            json!({"jsonrpc":"2.0","id":1,"method":"sampling/createMessage"}),
        ] {
            assert!(matches!(
                decode(&message, 1, &cx),
                Err(Failure::Protocol(_))
            ));
        }
    }
}
