//! Local authorization server and protected MCP endpoint; no live services.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn serve(marker: &str, scenario: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    sys::fs::write_private(std::path::Path::new(marker), origin.as_bytes()).unwrap();
    std::fs::write(format!("{marker}.refreshes"), "0").unwrap();
    let state = Arc::new(Mutex::new(State {
        origin,
        scenario: scenario.into(),
        marker: marker.into(),
        challenge: String::new(),
        scope: String::new(),
        tokens: 0,
        calls: 0,
        refreshes: 0,
        expired: false,
        gate: None,
        old_requests: 0,
    }));
    for stream in listener.incoming() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || exchange(stream.unwrap(), &state));
    }
}

fn exchange(mut stream: TcpStream, state: &Mutex<State>) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let head = String::from_utf8(bytes).unwrap();
    let target = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(|n| n.parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    if target == "/wait-expiry" {
        std::thread::sleep(Duration::from_millis(1250));
    }
    let (gate, late) = {
        let mut state = state.lock().unwrap();
        if target == "/mcp" && state.expired {
            state.old_requests += 1;
            std::fs::write(
                format!("{}.old-requests", state.marker),
                state.old_requests.to_string(),
            )
            .unwrap();
            (
                state.gate.clone(),
                state.old_requests == 1 && state.scenario != "rotating-cancel",
            )
        } else {
            (None, false)
        }
    };
    if let Some(gate) = gate {
        gate.wait();
    }
    if late {
        std::thread::sleep(Duration::from_millis(500));
    }
    if target == "/token" && body.starts_with(b"grant_type=refresh_token") {
        let (scenario, marker) = {
            let state = state.lock().unwrap();
            (state.scenario.clone(), state.marker.clone())
        };
        if scenario.starts_with("rotating") {
            std::fs::write(format!("{marker}.refresh-start"), b"ready").unwrap();
            if scenario == "rotating-cancel" {
                while !std::path::Path::new(&format!("{marker}.release")).exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            } else {
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    }
    let (status, headers, value) = state.lock().unwrap().reply(target, &head, &body);
    let body = serde_json::to_vec(&value).unwrap();
    write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
    let _ = stream.write_all(&body);
}

struct State {
    origin: String,
    scenario: String,
    marker: String,
    challenge: String,
    scope: String,
    tokens: usize,
    calls: usize,
    refreshes: usize,
    expired: bool,
    gate: Option<Arc<Barrier>>,
    old_requests: usize,
}

impl State {
    fn reply(&mut self, target: &str, head: &str, body: &[u8]) -> (u16, String, Value) {
        let origin = &self.origin;
        if target == "/wait-expiry" {
            self.expired = true;
            self.gate = None;
            self.old_requests = 0;
            std::fs::write(format!("{}.old-requests", self.marker), "0").unwrap();
            return (200, String::new(), json!({}));
        }
        if let Some(count) = target.strip_prefix("/expire/") {
            self.expired = true;
            self.old_requests = 0;
            self.gate = Some(Arc::new(Barrier::new(count.parse().unwrap())));
            return (200, String::new(), json!({}));
        }
        if target == "/mcp" {
            self.calls += 1;
            let request: Value = serde_json::from_slice(body).unwrap();
            let later_page = request["params"]["cursor"] == "next";
            let authorized = head.lines().any(|line| {
                line.eq_ignore_ascii_case(&format!("authorization: Bearer access-{}", self.tokens))
            });
            let needs_auth = (self.expired || !authorized)
                || (matches!(self.scenario.as_str(), "refresh" | "revoked") && self.calls == 3);
            let step_up = (matches!(
                self.scenario.as_str(),
                "stepup" | "declined-stepup" | "failed-stepup"
            ) && self.calls == 3)
                || (self.scenario == "during-call-stepup" && self.calls == 2)
                || (self.scenario == "rotating-stepup" && self.expired)
                || (matches!(self.scenario.as_str(), "paginated" | "cache-paginated")
                    && later_page
                    && !self.scope.split_whitespace().any(|scope| scope == "write"));
            if needs_auth || step_up {
                let (status, extra) = if step_up {
                    (403, ", error=\"insufficient_scope\", scope=\"write\"")
                } else if self.scenario == "no-scope" {
                    (401, "")
                } else {
                    (401, ", scope=\"read\"")
                };
                return (
                    status,
                    format!(
                        "WWW-Authenticate: Basic realm=\"other\", Bearer resource_metadata=\"{origin}/metadata\"{extra}\r\n"
                    ),
                    json!({"error":"authorization_required"}),
                );
            }
            if self.scenario.starts_with("rotating") && self.tokens >= 2 {
                let files = std::path::Path::new(&self.marker)
                    .parent()
                    .unwrap()
                    .join("credentials");
                let persisted = std::fs::read_dir(files).unwrap().any(|entry| {
                    let bytes = std::fs::read(entry.unwrap().path()).unwrap();
                    serde_json::from_slice::<Value>(&bytes).is_ok_and(|saved| {
                        saved["access_token"] == format!("access-{}", self.tokens)
                            && saved["refresh_token"] == format!("fixture-refresh-{}", self.tokens)
                    })
                });
                if !persisted {
                    return (
                        500,
                        String::new(),
                        json!({"error":"replacement_not_persisted"}),
                    );
                }
            }
            let result = self.list_result(&request);
            return (
                200,
                String::new(),
                json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
            );
        }
        if target == "/metadata" {
            let mut metadata = json!({"resource":format!("{origin}/mcp"),"authorization_servers":[format!("{origin}/issuer")],"scopes_supported":["read"]});
            if self.scenario == "no-scope" {
                metadata.as_object_mut().unwrap().remove("scopes_supported");
            }
            return (200, String::new(), metadata);
        }
        if target == "/.well-known/oauth-authorization-server/issuer" {
            return (
                200,
                String::new(),
                json!({"issuer":format!("{origin}/{}", if self.scenario == "wrong-metadata-issuer" {"wrong"} else {"issuer"}),"authorization_endpoint":format!("{origin}/authorize"),"token_endpoint":format!("{origin}/token"),"code_challenge_methods_supported":["S256"],"client_id_metadata_document_supported":self.scenario != "preregistered","authorization_response_iss_parameter_supported":true}),
            );
        }
        if let Some(query) = target.strip_prefix("/authorize?") {
            return self.authorize(query);
        }
        if target == "/token" {
            return self.token(body);
        }
        (404, String::new(), json!({}))
    }

    fn list_result(&self, request: &Value) -> Value {
        let later_page = request["params"]["cursor"] == "next";
        let mut result =
            if matches!(self.scenario.as_str(), "paginated" | "cache-paginated") && !later_page {
                json!({"resultType":"complete","tools":[],"nextCursor":"next"})
            } else {
                json!({"resultType":"complete","tools":[]})
            };
        if self.scenario.starts_with("cache-") {
            if request["method"] == "tools/list" {
                use std::io::Write;
                writeln!(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(format!("{}.lists", self.marker))
                        .unwrap(),
                    "{} {later_page}",
                    self.tokens
                )
                .unwrap();
                result["tools"] = json!([{"name":if later_page {"second"} else {"echo"},"description":format!("generation {}", self.tokens),"inputSchema":{"type":"object"}}]);
            }
            result["ttlMs"] = json!(300_000);
            result["cacheScope"] = json!("private");
        }
        result
    }

    fn authorize(&mut self, query: &str) -> (u16, String, Value) {
        let params: BTreeMap<_, _> = form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        // Resolve the default identity locally, without fetching the public placeholder.
        if self.scenario == "preregistered" {
            assert_eq!(params["client_id"], "fixture-client");
        } else {
            let document = local_document(&params["client_id"]);
            assert_eq!(document["client_id"], params["client_id"]);
        }
        let redirect: http::Uri = params["redirect_uri"].parse().unwrap();
        assert_eq!(redirect.host(), Some("127.0.0.1"));
        assert_eq!(redirect.path(), "/oauth/callback");
        assert_eq!(params["resource"], format!("{}/mcp", self.origin));
        assert_eq!(params["code_challenge_method"], "S256");
        self.challenge.clone_from(&params["code_challenge"]);
        if self.scenario == "no-scope" {
            assert!(!params.contains_key("scope"));
        }
        let denied = matches!(
            self.scenario.as_str(),
            "rotating" | "rotating-denied" | "failed-stepup"
        ) && self.tokens > 0;
        if !denied {
            self.scope = params.get("scope").cloned().unwrap_or_default();
        }
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                (
                    if denied { "error" } else { "code" },
                    if denied {
                        "access_denied"
                    } else {
                        "fixture-code"
                    },
                ),
                ("state", &params["state"]),
                (
                    "iss",
                    &format!(
                        "{}/{}",
                        self.origin,
                        if self.scenario == "wrong-response-issuer" {
                            "wrong"
                        } else {
                            "issuer"
                        }
                    ),
                ),
            ])
            .finish();
        (
            302,
            format!("Location: {}?{query}\r\n", params["redirect_uri"]),
            json!({}),
        )
    }

    fn token(&mut self, body: &[u8]) -> (u16, String, Value) {
        let params: BTreeMap<_, _> = form_urlencoded::parse(body).into_owned().collect();
        assert_eq!(params["resource"], format!("{}/mcp", self.origin));
        assert_eq!(
            params["client_id"],
            if self.scenario == "preregistered" {
                "fixture-client"
            } else {
                ext_mcp::OAUTH_CLIENT_ID
            }
        );
        if params["grant_type"] == "refresh_token" && self.scenario.starts_with("rotating") {
            self.refreshes += 1;
            std::fs::write(
                format!("{}.refreshes", self.marker),
                self.refreshes.to_string(),
            )
            .unwrap();
            if self.scenario == "rotating-transient" && self.refreshes == 1 {
                self.gate = None;
                return (
                    503,
                    String::new(),
                    json!({"error":"temporarily_unavailable"}),
                );
            }
            if self.scenario == "rotating-revoked"
                || self.scenario == "rotating-denied"
                || params["refresh_token"] != format!("fixture-refresh-{}", self.tokens)
            {
                return (400, String::new(), json!({"error":"invalid_grant"}));
            }
        }
        if self.scenario == "revoked" && params["grant_type"] == "refresh_token" {
            return (400, String::new(), json!({"error":"invalid_grant"}));
        }
        if params["grant_type"] == "authorization_code" {
            assert_eq!(params["code"], "fixture-code");
            assert_eq!(
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(Sha256::digest(params["code_verifier"].as_bytes())),
                self.challenge
            );
        } else if !self.scenario.starts_with("rotating") {
            assert_eq!(params["refresh_token"], "fixture-refresh");
        }
        self.tokens += 1;
        self.expired = false;
        std::fs::write(format!("{}.tokens", self.marker), self.tokens.to_string()).unwrap();
        std::fs::write(format!("{}.scope", self.marker), &self.scope).unwrap();
        (
            200,
            String::new(),
            json!({"access_token":format!("access-{}", self.tokens),"token_type":"Bearer","refresh_token":if self.scenario.starts_with("rotating") {format!("fixture-refresh-{}", self.tokens)} else {"fixture-refresh".into()},"scope":self.scope,"expires_in":if matches!(self.scenario.as_str(), "rotating-expiry" | "cache-expiry") && self.tokens == 1 {1} else {3600}}),
        )
    }
}

fn local_document(id: &str) -> Value {
    assert_eq!(id, ext_mcp::OAUTH_CLIENT_ID);
    // RFC 8252 native loopback redirect matching permits an ephemeral port.
    json!({"client_id":id,"client_name":"a","application_type":"native","redirect_uris":["http://127.0.0.1/oauth/callback"],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"})
}
