use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ext::host::Registry;
use ext::testing::Rig;
use ext::{Answer, Configured, Extension, Refused, Registrar, Table};
use ext_mcp::{Mcp, Server, Transport};
use serde_json::json;

struct Peer(Child);
impl Drop for Peer {
    fn drop(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

struct Browser {
    credentials: sys::credentials::Credentials,
    approvals: Arc<AtomicUsize>,
    threads: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}
impl Extension for Browser {
    const NAME: &'static str = "oauth-test-browser";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        let approvals = Arc::clone(&self.approvals);
        let threads = Arc::clone(&self.threads);
        r.provide(Arc::new(self.credentials.clone()))
            .on::<ext::point::Approve>(move |_, call| {
                if call.tool != "mcp_authorize" {
                    return Answer::Now(None);
                }
                let input: serde_json::Value = call.input.parse().unwrap();
                let url = input["url"].as_str().unwrap().to_owned();
                approvals.fetch_add(1, Ordering::SeqCst);
                threads.lock().unwrap().push(std::thread::spawn(move || {
                    let response = get(&url);
                    let redirect = response
                        .lines()
                        .find_map(|line| line.strip_prefix("Location: "))
                        .unwrap();
                    let response = get(redirect);
                    assert!(response.starts_with("HTTP/1.1 200"));
                }));
                Answer::Now(Some(true))
            });
    }
}
struct Settings(sys::config::Table);
impl Table for Settings {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn text(&self, key: &str) -> Result<Option<&str>, Refused> {
        self.0.text(key).map_err(|e| Refused(e.to_string()))
    }
    fn whole(&self, key: &str) -> Result<Option<u64>, Refused> {
        self.0.whole(key).map_err(|e| Refused(e.to_string()))
    }
    fn flag(&self, key: &str) -> Result<Option<bool>, Refused> {
        self.0.flag(key).map_err(|e| Refused(e.to_string()))
    }
    fn list(&self, key: &str) -> Result<Option<&[String]>, Refused> {
        self.0.list(key).map_err(|e| Refused(e.to_string()))
    }
}

fn get(url: &str) -> String {
    let uri: http::Uri = url.parse().unwrap();
    let mut stream = TcpStream::connect(uri.authority().unwrap().as_str()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        uri.path_and_query().unwrap(),
        uri.authority().unwrap()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn exercise(scenario: &str) {
    let root = Rig::new("oauth-home", Registry::default());
    let marker = root.dir.join("endpoint");
    let _peer = Peer(
        Command::new(env!("CARGO_BIN_EXE_mcp-fixture"))
            .arg("--http")
            .arg(&marker)
            .arg(scenario)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let origin = loop {
        if let Ok(url) = std::fs::read_to_string(&marker) {
            break url;
        }
        assert!(Instant::now() < deadline, "fixture did not bind");
        std::thread::sleep(Duration::from_millis(5));
    };
    let browser = Browser {
        credentials: sys::credentials::Credentials::isolated(root.dir.join("credentials")),
        approvals: Arc::default(),
        threads: Arc::default(),
    };
    let mcp = if matches!(scenario, "preregistered" | "wrong-registration-issuer") {
        let path = root.dir.join("config.toml");
        let issuer = if scenario == "wrong-registration-issuer" {
            "wrong"
        } else {
            "issuer"
        };
        std::fs::write(&path, format!("[ext.mcp]\nservers = [\"fixture\"]\nfixture_url = \"{origin}/mcp\"\nfixture_client_id = \"fixture-client\"\nfixture_issuer = \"{origin}/{issuer}\"\n")).unwrap();
        Mcp::configure(&Settings(
            sys::Config::read(&path).unwrap().ext.table("mcp"),
        ))
        .unwrap()
    } else {
        Mcp::new(
            vec![Server {
                name: "fixture".parse().unwrap(),
                description: String::new(),
                transport: Transport::Http(format!("{origin}/mcp")),
                env: BTreeMap::default(),
            }],
            Duration::from_secs(10),
        )
        .unwrap()
    };
    let rig = Rig::new(
        "oauth",
        Registry::default()
            .with(&mcp)
            .unwrap()
            .with(&browser)
            .unwrap(),
    );
    let result = rig.done("mcp_list", &json!({"server":"fixture"}));
    if scenario.starts_with("wrong-") {
        assert!(
            result["failed"]["transport"]
                .as_str()
                .unwrap()
                .contains("issuer mismatch"),
            "{result}"
        );
        assert!(!marker.with_extension("tokens").exists());
    } else {
        assert_eq!(
            result["ok"],
            json!([]),
            "authorization must finish: {result}"
        );
        let result = rig.done("mcp_list", &json!({"server":"fixture"}));
        assert_eq!(
            result["ok"],
            json!([]),
            "later operation must finish: {result}"
        );
        let expected = usize::from(matches!(
            scenario,
            "stepup" | "during-call-stepup" | "revoked"
        )) + 1;
        assert_eq!(browser.approvals.load(Ordering::SeqCst), expected);
        assert_eq!(
            std::fs::read_to_string(marker.with_extension("tokens")).unwrap(),
            if matches!(scenario, "basic" | "preregistered") {
                "1"
            } else {
                "2"
            }
        );
        if matches!(scenario, "stepup" | "during-call-stepup") {
            assert_eq!(
                std::fs::read_to_string(marker.with_extension("scope")).unwrap(),
                "read write"
            );
        }
    }
    for browser in browser.threads.lock().unwrap().drain(..) {
        browser.join().unwrap();
    }
    let log = rig.log.lock().unwrap();
    let log = serde_json::to_string(&*log).unwrap();
    for secret in [
        "fixture-refresh",
        "fixture-code",
        "code_verifier",
        "access-1",
        "access-2",
    ] {
        assert!(!log.contains(secret), "secret reached the session log");
    }
}

#[test]
fn a_protected_http_server_is_authorized_without_per_server_client_configuration() {
    exercise("basic");
}
#[test]
fn an_expired_token_is_refreshed_without_reopening_authorization() {
    exercise("refresh");
}
#[test]
fn a_revoked_refresh_token_requires_new_authorization() {
    exercise("revoked");
}
#[test]
fn step_up_authorization_preserves_previously_granted_scopes() {
    exercise("stepup");
}
#[test]
fn discovery_rejects_a_metadata_issuer_that_differs_from_its_url() {
    exercise("wrong-metadata-issuer");
}
#[test]
fn a_wrong_redirect_issuer_never_reaches_the_token_endpoint() {
    exercise("wrong-response-issuer");
}

#[test]
fn each_scope_upgrade_in_one_operation_gets_its_own_authorization_question() {
    exercise("during-call-stepup");
}

#[test]
fn a_pre_registered_client_id_is_used_when_metadata_documents_are_unavailable() {
    exercise("preregistered");
}

#[test]
fn a_pre_registered_client_id_cannot_be_sent_to_another_issuer() {
    exercise("wrong-registration-issuer");
}
