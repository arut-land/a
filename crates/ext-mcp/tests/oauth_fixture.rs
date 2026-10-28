//! Local authorization server and protected MCP endpoint; no live services.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn serve(marker: &str, scenario: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    std::fs::write(marker, &origin).unwrap();
    let mut state = State {
        origin,
        scenario,
        marker,
        challenge: String::new(),
        scope: String::new(),
        tokens: 0,
        calls: 0,
    };
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
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
        let (status, headers, value) = state.reply(target, &head, &body);
        let body = serde_json::to_vec(&value).unwrap();
        write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(&body).unwrap();
    }
}

struct State<'a> {
    origin: String,
    scenario: &'a str,
    marker: &'a str,
    challenge: String,
    scope: String,
    tokens: usize,
    calls: usize,
}

impl State<'_> {
    fn reply(&mut self, target: &str, head: &str, body: &[u8]) -> (u16, String, Value) {
        let origin = &self.origin;
        if target == "/mcp" {
            self.calls += 1;
            let authorized = head.lines().any(|line| {
                line.eq_ignore_ascii_case(&format!("authorization: Bearer access-{}", self.tokens))
            });
            let needs_auth =
                !authorized || (matches!(self.scenario, "refresh" | "revoked") && self.calls == 3);
            let step_up = (self.scenario == "stepup" && self.calls == 3)
                || (self.scenario == "during-call-stepup" && self.calls == 2);
            if needs_auth || step_up {
                let (status, extra) = if step_up {
                    (403, ", error=\"insufficient_scope\", scope=\"write\"")
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
            let request: Value = serde_json::from_slice(body).unwrap();
            return (
                200,
                String::new(),
                json!({"jsonrpc":"2.0","id":request["id"],"result":{"resultType":"complete","tools":[]}}),
            );
        }
        if target == "/metadata" {
            return (
                200,
                String::new(),
                json!({"resource":format!("{origin}/mcp"),"authorization_servers":[format!("{origin}/issuer")],"scopes_supported":["read"]}),
            );
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
        self.scope.clone_from(&params["scope"]);
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("code", "fixture-code"),
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
        } else {
            assert_eq!(params["refresh_token"], "fixture-refresh");
        }
        self.tokens += 1;
        std::fs::write(format!("{}.tokens", self.marker), self.tokens.to_string()).unwrap();
        std::fs::write(format!("{}.scope", self.marker), &self.scope).unwrap();
        (
            200,
            String::new(),
            json!({"access_token":format!("access-{}", self.tokens),"token_type":"Bearer","refresh_token":"fixture-refresh","scope":self.scope}),
        )
    }
}

fn local_document(id: &str) -> Value {
    assert_eq!(id, ext_mcp::OAUTH_CLIENT_ID);
    // RFC 8252 native loopback redirect matching permits an ephemeral port.
    json!({"client_id":id,"client_name":"a","redirect_uris":["http://127.0.0.1/oauth/callback"],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"})
}
