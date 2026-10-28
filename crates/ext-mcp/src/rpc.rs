//! The protocol subset shared by the two transports. It never retries
//! requests: a server may have effected a call before losing its reply.

use std::future::{Future, poll_fn};
use std::sync::atomic::{AtomicU64, Ordering};

use ext::Cx;
use llm::Http;
use serde_json::{Value, json};

use crate::Failure;

pub(crate) const LIMIT: usize = 1024 * 1024;
pub(crate) const VERSION: &str = "2026-07-28";

pub(crate) fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The boot component prevents a resumed exchange from reusing its pre-crash ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RequestId {
    boot: u128,
    sequence: u64,
}
impl RequestId {
    pub(crate) fn new() -> Self {
        static BOOT: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
        Self {
            boot: *BOOT.get_or_init(|| protocol::SessionId::new().0.as_u128()),
            sequence: next_id(),
        }
    }
    pub(crate) fn read(value: &Value) -> Option<Self> {
        let (boot, sequence) = value.as_str()?.split_once(':')?;
        if boot.len() != 32
            || !boot
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || sequence.is_empty()
            || !sequence.bytes().all(|b| b.is_ascii_digit())
            || sequence.len() > 1 && sequence.starts_with('0')
        {
            return None;
        }
        Some(Self {
            boot: u128::from_str_radix(boot, 16).ok()?,
            sequence: sequence.parse().ok()?,
        })
    }
    #[cfg(test)]
    pub(crate) const fn test(sequence: u64) -> Self {
        Self { boot: 0, sequence }
    }
}
impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}:{}", self.boot, self.sequence)
    }
}
impl serde::Serialize for RequestId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

pub(crate) struct Reply {
    pub value: Value,
    pub authorization: Option<crate::oauth::GenerationId>,
    pub received: std::time::Instant,
    pub changed: bool,
}

impl Reply {
    fn received(
        value: Value,
        authorization: Option<crate::oauth::GenerationId>,
        cx: &Cx,
        incoming: &crate::incoming::Incoming,
    ) -> Self {
        Self {
            value,
            authorization,
            received: cx.service::<dyn rt::Runtime>().expect("MCP runtime").now(),
            changed: incoming.changed,
        }
    }
}

pub(crate) fn request(id: RequestId, method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": VERSION,
        "io.modelcontextprotocol/clientCapabilities": {"elicitation":{"form":{},"url":{}}},
        "io.modelcontextprotocol/clientInfo": {"name":"a","version":env!("CARGO_PKG_VERSION")},
        "progressToken": id
    });
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

pub(crate) fn decode(message: &Value, id: Option<RequestId>) -> Result<Option<Value>, Failure> {
    if message["jsonrpc"] != "2.0" || !message.is_object() {
        return Err(Failure::Protocol("expected a JSON-RPC 2.0 object".into()));
    }
    if message.get("id").is_none() {
        message["method"]
            .as_str()
            .ok_or_else(|| Failure::Protocol("notification has no method".into()))?;
        return Ok(None);
    }
    if RequestId::read(&message["id"]).is_none_or(|response| Some(response) != id)
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
        Some("complete" | "input_required") => Ok(Some(result)),
        _ => Err(Failure::Protocol("unknown or missing resultType".into())),
    }
}

#[cfg(test)]
pub(crate) async fn http(
    url: &str,
    method: &str,
    params: Value,
    headers: http::HeaderMap,
    cx: &Cx,
) -> Result<Value, Failure> {
    let client = cx
        .service::<dyn transport::HttpClients>()
        .expect("engine HTTP clients")
        .client(transport::RedirectPolicy::Refuse);
    exchange(&*client, url, method, params, headers, cx).await
}

pub(crate) fn client(connection: crate::oauth::Connection, cx: &Cx) -> crate::oauth::Client {
    crate::oauth::Client {
        inner: cx
            .service::<dyn transport::HttpClients>()
            .expect("engine HTTP clients")
            .client(transport::RedirectPolicy::Refuse),
        cx: cx.clone(),
        connection,
    }
}

pub(crate) async fn http_authorized(
    url: &str,
    method: &str,
    params: Value,
    headers: http::HeaderMap,
    connection: crate::oauth::Connection,
    cx: &Cx,
) -> Result<Reply, Failure> {
    let timeout = connection.timeout;
    let client = client(connection, cx);
    guarded(
        timeout,
        cx,
        exchange_reply(&client, url, method, params, headers, cx),
    )
    .await
}

pub(crate) async fn guarded<T>(
    timeout: std::time::Duration,
    cx: &Cx,
    work: impl Future<Output = Result<T, Failure>>,
) -> Result<T, Failure> {
    let runtime = cx.service::<dyn rt::Runtime>().expect("MCP runtime");
    rt::or(work, async {
        rt::or(
            async {
                cx.cancel.wait().await;
                Err(Failure::Cancelled)
            },
            async {
                runtime.sleep(timeout).await;
                Err(Failure::Timeout)
            },
        )
        .await
    })
    .await
}

#[cfg(test)]
async fn exchange(
    client: &dyn Http,
    url: &str,
    method: &str,
    params: Value,
    headers: http::HeaderMap,
    cx: &Cx,
) -> Result<Value, Failure> {
    exchange_reply(client, url, method, params, headers, cx)
        .await
        .map(|reply| reply.value)
}

async fn exchange_reply(
    client: &dyn Http,
    url: &str,
    method: &str,
    params: Value,
    headers: http::HeaderMap,
    cx: &Cx,
) -> Result<Reply, Failure> {
    let id = RequestId::new();
    let message = request(id, method, params);
    let mut incoming = crate::incoming::Incoming::new(id);
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
    let authorization = response
        .extensions()
        .get::<crate::oauth::GenerationId>()
        .cloned();
    if status.is_redirection() {
        return Err(Failure::Redirect {
            status: status.as_u16(),
        });
    }
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
    while let Some(chunk) = incoming
        .wait(cx, poll_fn(|cx| body.as_mut().poll_next(cx)))
        .await
    {
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
                        let result = incoming.decode(&value, event.len())?;
                        event.clear();
                        incoming.publish(cx, result.is_some());
                        if let Some(result) = result {
                            return http_result(status, result)
                                .map(|value| Reply::received(value, authorization, cx, &incoming));
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
    let result = incoming
        .decode(&value, pending.len())?
        .ok_or_else(|| Failure::Protocol(format!("HTTP {status}: no response")))?;
    incoming.publish(cx, true);
    http_result(status, result).map(|value| Reply::received(value, authorization, cx, &incoming))
}

fn http_result(status: http::StatusCode, result: Value) -> Result<Value, Failure> {
    if status.is_success() {
        Ok(result)
    } else {
        Err(Failure::Transport(format!("HTTP {status}")))
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
        let id = RequestId::test(1);
        let valid = json!({"jsonrpc":"2.0","id":id,"result":{"resultType":"complete"}});
        assert_eq!(decode(&valid, Some(id)), Ok(Some(valid["result"].clone())));
        for (field, value) in [
            ("jsonrpc", json!("1.0")),
            ("id", json!(RequestId::test(2))),
            ("error", json!({"code":-1,"message":"error"})),
            ("result", json!({})),
            ("method", json!("sampling/createMessage")),
        ] {
            let mut message = valid.clone();
            message[field] = value;
            assert!(
                matches!(decode(&message, Some(id)), Err(Failure::Protocol(_))),
                "{message}"
            );
        }
    }

    #[test]
    fn sse_notifications_share_the_stdio_request_budget() {
        struct Flood;
        impl Http for Flood {
            fn send(
                &self,
                _: http::Request<llm::http::Payload>,
            ) -> llm::http::BoxFuture<'static, std::io::Result<llm::http::Response>> {
                let event = json!({"jsonrpc":"2.0","method":"notifications/unknown"});
                let bytes = format!("data: {event}\n\n").repeat(257).into_bytes();
                Box::pin(std::future::ready(Ok(http::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(llm::http::body([bytes]))
                    .unwrap())))
            }
        }
        let rig = Rig::new("mcp-sse-budget", Registry::default());
        let result = rt::block_on(exchange(
            &Flood,
            "http://fixture.invalid/mcp",
            "tools/list",
            json!({}),
            http::HeaderMap::new(),
            &rig.cx(),
        ));
        assert_eq!(result, Err(Failure::Limit));
        assert!(rig.progress.lock().unwrap().is_empty());
    }

    #[test]
    fn mcp_redirects_are_always_refused() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            // Closing with an unread request body can reset TCP and discard
            // the redirect before the client receives it.
            let head = std::str::from_utf8(&head).unwrap();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            stream.read_exact(&mut vec![0; length]).unwrap();
            stream.write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                if listener.accept().is_ok() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        });
        let rig = Rig::new("mcp-same-origin", Registry::default());
        let result = rt::block_on(http(
            &url,
            "tools/call",
            json!({"name":"echo","arguments":{}}),
            http::HeaderMap::new(),
            &rig.cx(),
        ));
        assert_eq!(result, Err(Failure::Redirect { status: 307 }));
        assert!(!server.join().unwrap(), "same-origin request was resent");
    }

    #[test]
    fn cross_host_redirects_never_receive_mirrored_arguments() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        fn receive(listener: &TcpListener, reply: &str) -> Option<String> {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut bytes = Vec::new();
                        let mut byte = [0];
                        while !bytes.ends_with(b"\r\n\r\n") {
                            if stream.read(&mut byte).unwrap() == 0 {
                                break;
                            }
                            bytes.push(byte[0]);
                        }
                        stream.write_all(reply.as_bytes()).unwrap();
                        return Some(String::from_utf8(bytes).unwrap());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return None;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }

        let source = TcpListener::bind("127.0.0.1:0").unwrap();
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://localhost:{}/mcp",
            source.local_addr().unwrap().port()
        );
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{}/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            target.local_addr().unwrap().port()
        );
        let source = std::thread::spawn(move || receive(&source, &redirect));
        let target = std::thread::spawn(move || {
            receive(
                &target,
                "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n",
            )
        });
        let rig = Rig::new("mcp-review-redirect", Registry::default());
        let mut headers = http::HeaderMap::new();
        headers.insert("mcp-param-tenant", "private-test-tenant".parse().unwrap());
        let _ = rt::block_on(http(
            &url,
            "tools/call",
            json!({"name":"echo","arguments":{"tenant":"private-test-tenant"}}),
            headers,
            &rig.cx(),
        ));
        assert!(source.join().unwrap().is_some());
        let received = target.join().unwrap();
        assert!(
            received.is_none(),
            "cross-host request leaked: {received:?}"
        );
    }
}
