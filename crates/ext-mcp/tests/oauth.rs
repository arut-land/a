use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ext::host::Registry;
use ext::testing::Rig;
use ext::{Configured, Extension, Refused, Registrar, Table};
use ext_mcp::{Mcp, Server, Transport};
use serde_json::json;

struct Peer(Child);
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        self.0.wait().unwrap();
    }
}

struct Browser {
    credentials: sys::credentials::Credentials,
    approvals: Arc<AtomicUsize>,
    threads: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    decline_write: bool,
}
struct BrowserQuestions {
    approvals: Arc<AtomicUsize>,
    threads: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    decline_write: bool,
}
impl ext::host::QuestionPort for BrowserQuestions {
    fn ask(
        &self,
        question: protocol::Question,
    ) -> ext::BoxFuture<'static, Result<protocol::QuestionAnswer, String>> {
        self.approvals.fetch_add(1, Ordering::SeqCst);
        let (key, prompt) = question.prompts.iter().next().unwrap();
        let protocol::Prompt::Url { url, .. } = prompt else {
            panic!("expected URL consent")
        };
        let uri: http::Uri = url.parse().unwrap();
        let decline = self.decline_write
            && form_urlencoded::parse(uri.query().unwrap().as_bytes())
                .any(|(k, v)| k == "scope" && v.split_whitespace().any(|s| s == "write"));
        if !decline {
            let url = url.clone();
            self.threads
                .lock()
                .unwrap()
                .push(std::thread::spawn(move || {
                    let response = get(&url);
                    let redirect = response
                        .lines()
                        .find_map(|line| line.strip_prefix("Location: "))
                        .unwrap();
                    assert!(get(redirect).starts_with("HTTP/1.1 200"));
                }));
        }
        let answer = protocol::QuestionAnswer {
            question: protocol::Seq::new(1).unwrap(),
            responses: BTreeMap::from([(
                key.clone(),
                if decline {
                    protocol::Response::Decline
                } else {
                    protocol::Response::accept()
                },
            )]),
        };
        Box::pin(std::future::ready(Ok(answer)))
    }
}
impl Extension for Browser {
    const NAME: &'static str = "oauth-test-browser";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.provide(Arc::new(self.credentials.clone()))
            .provide::<dyn ext::host::QuestionPort>(Arc::new(BrowserQuestions {
                approvals: self.approvals.clone(),
                threads: self.threads.clone(),
                decline_write: self.decline_write,
            }));
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

fn configured(root: &Rig, origin: &str, scenario: &str) -> Mcp {
    if matches!(scenario, "preregistered" | "wrong-registration-issuer") {
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
    }
}

fn serve(marker: &Path, scenario: &str) -> (Peer, String) {
    let peer = Peer(
        Command::new(env!("CARGO_BIN_EXE_mcp-fixture"))
            .arg("--http")
            .arg(marker)
            .arg(scenario)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let origin = loop {
        if let Ok(url) = std::fs::read_to_string(marker) {
            break url;
        }
        assert!(Instant::now() < deadline, "fixture did not bind");
        std::thread::sleep(Duration::from_millis(5));
    };
    (peer, origin)
}

fn exercise(scenario: &str) {
    let root = Rig::new("oauth-home", Registry::default());
    let marker = root.dir.join("endpoint");
    let (_peer, origin) = serve(&marker, scenario);
    let browser = Browser {
        credentials: sys::credentials::Credentials::isolated(root.dir.join("credentials")),
        approvals: Arc::default(),
        threads: Arc::default(),
        decline_write: false,
    };
    let mcp = configured(&root, &origin, scenario);
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
        if scenario == "cached" {
            std::fs::remove_dir_all(root.dir.join("credentials")).unwrap();
            std::fs::write(root.dir.join("credentials"), b"store unavailable").unwrap();
        }
        let result = rig.done("mcp_list", &json!({"server":"fixture"}));
        assert_eq!(
            result["ok"],
            json!([]),
            "later operation must finish: {result}"
        );
        if scenario == "basic" {
            let fresh = Rig::new(
                "oauth-restarted",
                Registry::default()
                    .with(&configured(&root, &origin, scenario))
                    .unwrap()
                    .with(&browser)
                    .unwrap(),
            );
            assert_eq!(
                fresh.done("mcp_list", &json!({"server":"fixture"}))["ok"],
                json!([])
            );
        }
        let expected = usize::from(matches!(
            scenario,
            "stepup" | "during-call-stepup" | "revoked"
        )) + 1;
        assert_eq!(browser.approvals.load(Ordering::SeqCst), expected);
        assert_eq!(
            std::fs::read_to_string(marker.with_extension("tokens")).unwrap(),
            if matches!(
                scenario,
                "refresh" | "revoked" | "stepup" | "during-call-stepup"
            ) {
                "2"
            } else {
                "1"
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

#[test]
fn authorization_omits_scope_when_neither_challenge_nor_metadata_names_one() {
    exercise("no-scope");
}

#[test]
fn successful_protected_calls_reuse_credentials_without_reopening_the_store() {
    exercise("cached");
}

#[test]
fn oauth_engine_process() {
    let Some(root) = std::env::var_os("A_MCP_TEST_HOME") else {
        return;
    };
    let home = sys::Home::from(std::path::PathBuf::from(root));
    let mut config = engine::Config::load(&home);
    config.agent = Some(ext_fixtures::ECHO.into());
    config.extensions = |config| {
        let table = Settings(config.ext.table("mcp"));
        let browser = Browser {
            credentials: sys::credentials::Credentials::isolated(
                config
                    .ext
                    .table("oauth-test-browser")
                    .text("credentials")
                    .unwrap()
                    .unwrap()
                    .into(),
            ),
            approvals: Arc::default(),
            threads: Arc::default(),
            decline_write: config
                .ext
                .table("oauth-test-browser")
                .flag("decline_write")
                .unwrap()
                .unwrap_or(false),
        };
        let registry = Registry::default()
            .with(&Mcp::configure(&table)?)?
            .with(&ext_fixtures::Fixtures)?
            .with(&browser)?;
        // Extension installation follows the daemon's exclusive record claim.
        println!("oauth-engine-ready");
        std::io::stdout().flush().unwrap();
        Ok(registry)
    };
    let listener = home.bind().unwrap();
    engine::serve(
        &listener,
        &home,
        &config,
        "oauth-test",
        engine::Start::default(),
    )
    .unwrap();
}

struct Engine {
    root: Rig,
    fixture: Peer,
    daemon: Peer,
    origin: String,
    output: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    fn new(scenario: &str) -> Self {
        let root = Rig::new("oauth-engine", Registry::default());
        let (fixture, origin) = serve(&root.dir.join("endpoint"), scenario);
        let home = sys::Home::from(root.dir.clone());
        std::fs::write(home.config_file(), format!(
            "[ext.mcp]\nservers=[\"fixture\"]\nfixture_url=\"{origin}/mcp\"\n[ext.oauth-test-browser]\ncredentials={}\ndecline_write={}\n",
            serde_json::to_string(&root.dir.join("credentials")).unwrap(),
            scenario == "declined-stepup",
        )).unwrap();
        let mut daemon = Peer(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "oauth_engine_process", "--nocapture"])
                .env("A_MCP_TEST_HOME", &root.dir)
                .env("A_HOME", &root.dir)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = daemon.0.stdout.take().unwrap();
        let (ready, acknowledged) = std::sync::mpsc::sync_channel(1);
        let output = std::thread::spawn(move || {
            let mut output = std::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                assert_ne!(
                    output.read_line(&mut line).unwrap(),
                    0,
                    "engine exited before readiness"
                );
                if line.trim_end().ends_with("oauth-engine-ready") {
                    break;
                }
                line.clear();
            }
            ready.send(()).unwrap();
            std::io::copy(&mut output, &mut std::io::sink()).unwrap();
        });
        acknowledged
            .recv_timeout(Duration::from_secs(10))
            .expect("engine did not acknowledge its owned listener");
        Self {
            root,
            fixture,
            daemon,
            origin,
            output: Some(output),
        }
    }

    fn stop(&mut self) {
        let mut client = connect(&sys::Home::from(self.root.dir.clone()));
        protocol::write_frame(&mut client, &protocol::ClientMsg::Shutdown).unwrap();
        assert!(self.daemon.0.wait().unwrap().success());
        self.output.take().unwrap().join().unwrap();
        self.fixture.0.kill().unwrap();
        self.fixture.0.wait().unwrap();
    }
}

struct Client {
    writer: sys::OwnedConnection,
    reader: std::io::BufReader<sys::OwnedConnection>,
    decline_write: bool,
}
impl Client {
    fn attach(root: &Path) -> Self {
        use protocol::{ClientMsg, DaemonMsg, read_frame, write_frame};
        let mut writer = connect(&sys::Home::from(root.to_owned()));
        let mut reader = std::io::BufReader::new(writer.try_clone().unwrap());
        write_frame(
            &mut writer,
            &ClientMsg::Attach {
                target: protocol::Target::New,
                workspace: Some(root.to_owned().try_into().unwrap()),
                since: 0,
                env: Some(protocol::Environment::default()),
                decl: protocol::ClientDecl::default(),
            },
        )
        .unwrap();
        loop {
            if matches!(
                read_frame::<DaemonMsg>(&mut reader).unwrap(),
                Some(DaemonMsg::Synced { .. })
            ) {
                break;
            }
        }
        Self {
            writer,
            reader,
            decline_write: std::fs::read_to_string(sys::Home::from(root.to_owned()).config_file())
                .unwrap()
                .contains("decline_write=true"),
        }
    }

    fn list(&mut self) -> (serde_json::Value, usize) {
        use protocol::{Body, ClientMsg, DaemonMsg, Outcome, read_frame, write_frame};
        write_frame(
            &mut self.writer,
            &ClientMsg::Input {
                segments: "call mcp_list {\"server\":\"fixture\"}".into(),
                mode: protocol::Mode::default(),
                force: false,
                client_seq: None,
            },
        )
        .unwrap();
        let mut parent = None;
        let mut authorizations = 0;
        loop {
            let message = read_frame::<DaemonMsg>(&mut self.reader)
                .unwrap()
                .expect("engine hung up");
            match message {
                DaemonMsg::Entry(entry) => match entry.body {
                    Body::ToolCall { tool, .. } if tool == "mcp_list" => {
                        parent = Some(protocol::CallId(entry.seq));
                    }
                    Body::Question { question } => {
                        authorizations += 1;
                        let (key, prompt) = question.prompts.iter().next().unwrap();
                        let protocol::Prompt::Url { url, .. } = prompt else {
                            panic!("URL question expected")
                        };
                        let uri: http::Uri = url.parse().unwrap();
                        let decline = self.decline_write
                            && form_urlencoded::parse(uri.query().unwrap().as_bytes()).any(
                                |(k, v)| k == "scope" && v.split_whitespace().any(|s| s == "write"),
                            );
                        let browser = (!decline).then(|| {
                            let url = url.clone();
                            std::thread::spawn(move || {
                                let response = get(&url);
                                let redirect = response
                                    .lines()
                                    .find_map(|line| line.strip_prefix("Location: "))
                                    .unwrap();
                                assert!(get(redirect).starts_with("HTTP/1.1 200"));
                            })
                        });
                        if let Some(browser) = browser {
                            browser.join().unwrap();
                        }
                        let answer = protocol::QuestionAnswer {
                            question: entry.seq,
                            responses: BTreeMap::from([(
                                key.clone(),
                                if decline {
                                    protocol::Response::Decline
                                } else {
                                    protocol::Response::accept()
                                },
                            )]),
                        };
                        write_frame(
                            &mut self.writer,
                            &ClientMsg::answer(protocol::CallId(entry.seq), answer),
                        )
                        .unwrap();
                    }
                    Body::ToolResult { call, outcome, .. } if Some(call) == parent => {
                        return (
                            match outcome {
                                Outcome::Done {
                                    output: protocol::Payload::Inline(output),
                                } => output.parse().unwrap(),
                                Outcome::Interrupted { .. } => json!({"interrupted":true}),
                                other => panic!("{other:?}"),
                            },
                            authorizations,
                        );
                    }
                    _ => {}
                },
                DaemonMsg::Error { message } => panic!("{message}"),
                _ => {}
            }
        }
    }
}

#[test]
fn paginated_authorization_names_are_unique_in_the_engine() {
    let mut engine = Engine::new("paginated");
    let (result, authorizations) = Client::attach(&engine.root.dir).list();
    engine.stop();
    assert_eq!(authorizations, 2, "{result}");
    assert_eq!(result["ok"], json!([]), "{result}");
    assert_eq!(
        std::fs::read_to_string(engine.root.dir.join("endpoint.scope")).unwrap(),
        "read write"
    );
}

fn rejected_step_up(scenario: &str) {
    let mut engine = Engine::new(scenario);
    let first = Client::attach(&engine.root.dir).list();
    let credential = std::fs::read_dir(engine.root.dir.join("credentials"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let saved = std::fs::read(&credential).unwrap();
    let upgrade = Client::attach(&engine.root.dir).list();
    let preserved = std::fs::read(&credential).ok().as_deref() == Some(saved.as_slice());
    let original_scope = Client::attach(&engine.root.dir).list();
    assert!(get(&format!("{}/expire/1", engine.origin)).starts_with("HTTP/1.1 200"));
    let refreshed = Client::attach(&engine.root.dir).list();
    let issued = std::fs::read_to_string(engine.root.dir.join("endpoint.tokens")).unwrap();
    let scope = std::fs::read_to_string(engine.root.dir.join("endpoint.scope")).unwrap();
    engine.stop();
    eprintln!(
        "step_up_preserved={preserved} original_scope_cards={} refresh_cards={}",
        original_scope.1, refreshed.1
    );
    assert_eq!(first, (json!({"ok":[]}), 1));
    assert!(
        upgrade.0["failed"]["transport"]
            .as_str()
            .unwrap()
            .contains("declined"),
        "{upgrade:?}"
    );
    assert_eq!(upgrade.1, 1);
    assert!(
        preserved,
        "a rejected upgrade must leave the stored grant unchanged"
    );
    assert_eq!(original_scope, (json!({"ok":[]}), 0));
    assert_eq!(refreshed, (json!({"ok":[]}), 0));
    assert_eq!(issued, "2");
    assert_eq!(scope, "read");
}

#[test]
fn declining_scope_step_up_preserves_previously_granted_access() {
    rejected_step_up("declined-stepup");
}

#[test]
fn failing_scope_step_up_preserves_previously_granted_access() {
    rejected_step_up("failed-stepup");
}

fn concurrent_refresh(scenario: &str) {
    let mut engine = Engine::new(scenario);
    let first = Client::attach(&engine.root.dir).list();
    assert_eq!(first, (json!({"ok":[]}), 1));
    let control = if scenario == "rotating-expiry" {
        "wait-expiry"
    } else {
        "expire/16"
    };
    assert!(get(&format!("{}/{control}", engine.origin)).starts_with("HTTP/1.1 200"));
    let workers: Vec<_> = (0..16)
        .map(|_| {
            let home = engine.root.dir.clone();
            std::thread::spawn(move || Client::attach(&home).list())
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(std::thread::JoinHandle::join)
        .collect();
    let retried = if scenario == "rotating-transient" {
        Some(Client::attach(&engine.root.dir).list())
    } else {
        None
    };
    engine.stop();
    let results: Vec<_> = results.into_iter().map(Result::unwrap).collect();
    let attempts = std::fs::read_to_string(engine.root.dir.join("endpoint.refreshes")).unwrap();
    eprintln!("oauth_refresh_attempts={attempts}");
    assert_eq!(
        attempts,
        if scenario == "rotating-transient" {
            "2"
        } else if scenario == "rotating-stepup" {
            "0"
        } else {
            "1"
        }
    );
    assert_eq!(
        results.iter().map(|(_, cards)| cards).sum::<usize>(),
        usize::from(matches!(
            scenario,
            "rotating-revoked" | "rotating-denied" | "rotating-stepup"
        ))
    );
    if scenario == "rotating-expiry" {
        assert_eq!(
            std::fs::read_to_string(engine.root.dir.join("endpoint.old-requests")).unwrap(),
            "0"
        );
    }
    if let Some(retried) = retried {
        assert_eq!(retried, (json!({"ok":[]}), 0));
    }
    for (result, _) in results {
        if scenario == "rotating-transient" {
            assert!(
                result["failed"]["transport"]
                    .as_str()
                    .unwrap()
                    .contains("token endpoint rejected"),
                "{result}"
            );
        } else if scenario == "rotating-denied" {
            assert!(
                result["failed"]["transport"]
                    .as_str()
                    .unwrap()
                    .contains("declined"),
                "{result}"
            );
        } else {
            assert_eq!(result["ok"], json!([]), "{result}");
        }
    }
}

#[test]
fn rotating_refresh_tokens_are_redeemed_exactly_once() {
    concurrent_refresh("rotating");
}

#[test]
fn a_revoked_shared_refresh_token_presents_one_authorization_for_all_sessions() {
    concurrent_refresh("rotating-revoked");
}

#[test]
fn a_declined_shared_authorization_resumes_every_waiter_with_the_same_failure() {
    concurrent_refresh("rotating-denied");
}

#[test]
fn concurrent_scope_step_up_uses_one_authorization_transaction() {
    concurrent_refresh("rotating-stepup");
}

#[test]
fn transient_refresh_failure_preserves_the_credential_for_a_later_retry() {
    concurrent_refresh("rotating-transient");
}

#[test]
fn token_expiry_joins_renewal_before_sending_an_expired_generation() {
    concurrent_refresh("rotating-expiry");
}

fn cancel_pending_tool(rig: &Rig, tool: &str, input: &serde_json::Value) -> serde_json::Value {
    let cx = rig.cx();
    let input = protocol::Json::encode(input).unwrap();
    let called = rt::block_on(rt::or(
        rig.registry.tools().run(tool, &input, None, &cx),
        async {
            // The first branch has already polled the real lookup.
            rig.cancel.set();
            std::future::pending().await
        },
    ));
    let protocol::Outcome::Done {
        output: protocol::Payload::Inline(output),
    } = called.outcome
    else {
        panic!("expected a typed MCP failure");
    };
    output.parse().unwrap()
}

#[test]
fn generation_waiters_keep_their_own_deadlines_and_cancellation_during_shared_renewal() {
    let root = Rig::new("generation-waiters", Registry::default());
    let marker = root.dir.join("endpoint");
    let (peer, origin) = serve(&marker, "rotating-cancel");
    let browser = Browser {
        credentials: sys::credentials::Credentials::isolated(root.dir.join("credentials")),
        approvals: Arc::default(),
        threads: Arc::default(),
        decline_write: false,
    };
    let authorizations = ext_mcp::Authorizations::default();
    let registry = |timeout| {
        let mcp = Mcp::with_authorizations(
            vec![Server {
                name: "fixture".parse().unwrap(),
                description: String::new(),
                transport: Transport::Http(format!("{origin}/mcp")),
                env: BTreeMap::default(),
            }],
            timeout,
            authorizations.clone(),
        )
        .unwrap();
        Registry::default()
            .with(&mcp)
            .unwrap()
            .with(&browser)
            .unwrap()
    };
    let long = Rig::new("long-renewal", registry(Duration::from_secs(10)));
    assert_eq!(
        long.done("mcp_list", &json!({"server":"fixture"}))["ok"],
        json!([])
    );
    assert!(get(&format!("{origin}/expire/1")).starts_with("HTTP/1.1 200"));
    let leader = std::thread::spawn(move || long.done("mcp_list", &json!({"server":"fixture"})));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.with_extension("refresh-start").exists() {
        assert!(Instant::now() < deadline, "renewal did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    let (send, receive) = std::sync::mpsc::channel();
    let mut workers = Vec::new();
    for cancelled in [false, true] {
        for tool in ["mcp_list", "mcp_schema", "mcp_call"] {
            let rig = Rig::new("short-waiter", registry(Duration::from_millis(100)));
            let send = send.clone();
            workers.push(std::thread::spawn(move || {
                let started = Instant::now();
                let input = json!({"server":"fixture","tool":"echo","arguments":{}});
                let result = if cancelled {
                    cancel_pending_tool(&rig, tool, &input)
                } else {
                    rig.done(tool, &input)
                };
                send.send((cancelled, tool, started.elapsed(), result))
                    .unwrap();
            }));
        }
    }
    drop(send);
    let mut results = Vec::new();
    while results.len() < workers.len() {
        match receive.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => results.push(result),
            Err(_) => break,
        }
    }
    let prompt = results.len();
    // Always release the real peer and join every owned caller, even on regression.
    std::fs::write(marker.with_extension("release"), b"ready").unwrap();
    for worker in workers {
        worker.join().unwrap();
    }
    results.extend(receive);
    let completed = leader.join().unwrap();
    for (cancelled, tool, elapsed, result) in &results {
        eprintln!(
            "generation_waiter cancelled={cancelled} tool={tool} elapsed_ms={:.3} result={result}",
            elapsed.as_secs_f64() * 1000.0
        );
    }
    assert_eq!(completed["ok"], json!([]), "{completed}");
    assert_eq!(
        std::fs::read_to_string(marker.with_extension("refreshes")).unwrap(),
        "1"
    );
    for thread in browser.threads.lock().unwrap().drain(..) {
        thread.join().unwrap();
    }
    drop(peer);
    assert_eq!(
        prompt, 6,
        "every short/cancelled waiter must return before the renewal is released"
    );
    for (cancelled, tool, _, result) in results {
        assert_eq!(
            result["failed"],
            if cancelled { "cancelled" } else { "timeout" },
            "{tool}: {result}"
        );
    }
}

#[test]
fn cancelling_the_refresh_starter_does_not_cancel_other_sessions_renewal() {
    let mut engine = Engine::new("rotating-cancel");
    assert_eq!(
        Client::attach(&engine.root.dir).list(),
        (json!({"ok":[]}), 1)
    );
    assert!(get(&format!("{}/expire/1", engine.origin)).starts_with("HTTP/1.1 200"));
    let mut leader = Client::attach(&engine.root.dir);
    let mut interrupt = leader.writer.try_clone().unwrap();
    let leader = std::thread::spawn(move || leader.list());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !engine.root.dir.join("endpoint.refresh-start").exists() {
        assert!(Instant::now() < deadline, "refresh did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    let workers: Vec<_> = (0..16)
        .map(|_| {
            let home = engine.root.dir.clone();
            std::thread::spawn(move || Client::attach(&home).list())
        })
        .collect();
    protocol::write_frame(
        &mut interrupt,
        &protocol::ClientMsg::Interrupt { client_seq: None },
    )
    .unwrap();
    let cancelled = leader.join();
    std::fs::write(engine.root.dir.join("endpoint.release"), b"ready").unwrap();
    let results: Vec<_> = workers
        .into_iter()
        .map(std::thread::JoinHandle::join)
        .collect();
    engine.stop();
    let cancelled = cancelled.unwrap().0;
    assert!(
        cancelled == json!({"interrupted":true}) || cancelled == json!({"failed":"cancelled"}),
        "{cancelled}"
    );
    for result in results {
        assert_eq!(result.unwrap(), (json!({"ok":[]}), 0));
    }
    assert_eq!(
        std::fs::read_to_string(engine.root.dir.join("endpoint.refreshes")).unwrap(),
        "1"
    );
}

#[test]
fn separately_composed_mcp_instances_share_the_same_credential_lifecycle() {
    let root = Rig::new("oauth-shared-owner", Registry::default());
    let marker = root.dir.join("endpoint");
    let (mut fixture, origin) = serve(&marker, "rotating");
    let browser = Browser {
        credentials: sys::credentials::Credentials::isolated(root.dir.join("credentials")),
        approvals: Arc::default(),
        threads: Arc::default(),
        decline_write: false,
    };
    let authorizations = ext_mcp::Authorizations::default();
    let rigs: Vec<_> = (0..2)
        .map(|_| {
            let mcp = Mcp::with_authorizations(
                vec![Server {
                    name: "fixture".parse().unwrap(),
                    description: String::new(),
                    transport: Transport::Http(format!("{origin}/mcp")),
                    env: BTreeMap::new(),
                }],
                Duration::from_secs(10),
                authorizations.clone(),
            )
            .unwrap();
            Arc::new(Rig::new(
                "oauth-shared-client",
                Registry::default()
                    .with(&mcp)
                    .unwrap()
                    .with(&browser)
                    .unwrap(),
            ))
        })
        .collect();
    assert_eq!(
        rigs[0].done("mcp_list", &json!({"server":"fixture"})),
        json!({"ok":[]})
    );
    assert!(get(&format!("{origin}/expire/2")).starts_with("HTTP/1.1 200"));
    let workers: Vec<_> = rigs
        .iter()
        .map(|rig| {
            let rig = Arc::clone(rig);
            std::thread::spawn(move || rig.done("mcp_list", &json!({"server":"fixture"})))
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(std::thread::JoinHandle::join)
        .collect();
    fixture.0.kill().unwrap();
    fixture.0.wait().unwrap();
    for result in results {
        assert_eq!(result.unwrap(), json!({"ok":[]}));
    }
    assert_eq!(
        std::fs::read_to_string(marker.with_extension("refreshes")).unwrap(),
        "1"
    );
    assert_eq!(browser.approvals.load(Ordering::SeqCst), 1);
    for browser in browser.threads.lock().unwrap().drain(..) {
        browser.join().unwrap();
    }
}

fn connect(home: &sys::Home) -> sys::OwnedConnection {
    use protocol::{ClientMsg, DaemonMsg, read_frame, write_frame};

    let mut writer = home.endpoint().connect().unwrap();
    writer
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    write_frame(
        &mut writer,
        &ClientMsg::Hello {
            proto: protocol::PROTO,
            build: "oauth-test".into(),
            caps: protocol::Caps::ALL,
            client: None,
            machine: None,
        },
    )
    .unwrap();
    assert!(matches!(
        read_frame::<DaemonMsg>(&mut writer).unwrap(),
        Some(DaemonMsg::Welcome { .. })
    ));
    writer
}

#[test]
fn private_discovery_tracks_the_actual_authorization_generation_after_refresh_and_expiry() {
    for scenario in ["cache-generations", "cache-expiry", "cache-paginated"] {
        let root = Rig::new(scenario, Registry::default());
        let marker = root.dir.join("endpoint");
        let (_peer, origin) = serve(&marker, scenario);
        let browser = Browser {
            credentials: sys::credentials::Credentials::isolated(root.dir.join("credentials")),
            approvals: Arc::default(),
            threads: Arc::default(),
            decline_write: false,
        };
        let mcp = configured(&root, &origin, scenario);
        let rig = Rig::new(
            "private-cache",
            Registry::default()
                .with(&mcp)
                .unwrap()
                .with(&browser)
                .unwrap(),
        );
        let first = rig.done("mcp_list", &json!({"server":"fixture"}));
        assert_eq!(
            first["ok"][0]["description"], "generation 1",
            "{scenario}: {first}"
        );
        if scenario == "cache-paginated" {
            assert_eq!(first["ok"][1]["description"], "generation 2");
            let second = rig.done("mcp_list", &json!({"server":"fixture"}));
            assert_eq!(second["ok"][0]["description"], "generation 2");
            assert_eq!(rig.done("mcp_list", &json!({"server":"fixture"})), second);
            assert_eq!(
                std::fs::read_to_string(marker.with_extension("lists"))
                    .unwrap()
                    .lines()
                    .count(),
                4
            );
        } else {
            assert_eq!(rig.done("mcp_list", &json!({"server":"fixture"})), first);
            let control = if scenario == "cache-expiry" {
                "wait-expiry"
            } else {
                "expire/1"
            };
            assert!(get(&format!("{origin}/{control}")).starts_with("HTTP/1.1 200"));
            if scenario == "cache-generations" {
                let called = rig.done(
                    "mcp_call",
                    &json!({"server":"fixture","tool":"echo","arguments":{}}),
                );
                assert!(called.get("ok").is_some(), "{called}");
            }
            let second = rig.done("mcp_list", &json!({"server":"fixture"}));
            assert_eq!(
                second["ok"][0]["description"], "generation 2",
                "{scenario}: {second}"
            );
            assert_eq!(rig.done("mcp_list", &json!({"server":"fixture"})), second);
            assert_eq!(
                std::fs::read_to_string(marker.with_extension("lists"))
                    .unwrap()
                    .lines()
                    .count(),
                2
            );
        }
        for thread in browser.threads.lock().unwrap().drain(..) {
            thread.join().unwrap();
        }
    }
}

#[test]
fn engine_readiness_owns_the_daemon_record_before_client_discovery() {
    let mut engine = Engine::new("paginated");
    let home = sys::Home::from(engine.root.dir.clone());
    let record = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.daemon_pid_file())
        .expect("readiness follows record publication");
    assert_eq!(
        sys::fs::try_lock(&record).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(record);
    engine.stop();
}

#[test]
fn interrupting_the_authorization_presenter_leaves_its_card_answerable_for_other_callers() {
    use protocol::{Body, ClientMsg, DaemonMsg, read_frame, write_frame};
    let mut engine = Engine::new("rotating");
    let mut presenter = Client::attach(&engine.root.dir);
    write_frame(
        &mut presenter.writer,
        &ClientMsg::Input {
            segments: "call mcp_list {\"server\":\"fixture\"}".into(),
            mode: protocol::Mode::default(),
            force: false,
            client_seq: None,
        },
    )
    .unwrap();
    let (id, question) = loop {
        if let Some(DaemonMsg::Entry(entry)) =
            read_frame::<DaemonMsg>(&mut presenter.reader).unwrap()
            && let Body::Question { question } = entry.body
        {
            break (entry.seq, question);
        }
    };
    write_frame(
        &mut presenter.writer,
        &ClientMsg::Interrupt { client_seq: None },
    )
    .unwrap();
    loop {
        if let Some(DaemonMsg::Entry(entry)) =
            read_frame::<DaemonMsg>(&mut presenter.reader).unwrap()
            && matches!(entry.body, Body::ToolResult { .. })
        {
            break;
        }
    }
    let home = engine.root.dir.clone();
    let waiter = std::thread::spawn(move || Client::attach(&home).list());
    let (key, protocol::Prompt::Url { url, .. }) = question.prompts.iter().next().unwrap() else {
        panic!("URL question");
    };
    let url = url.clone();
    let browser = std::thread::spawn(move || {
        let response = get(&url);
        let redirect = response
            .lines()
            .find_map(|line| line.strip_prefix("Location: "))
            .unwrap();
        assert!(get(redirect).starts_with("HTTP/1.1 200"));
    });
    browser.join().unwrap();
    assert!(!engine.root.dir.join("endpoint.tokens").exists());
    write_frame(
        &mut presenter.writer,
        &ClientMsg::answer(
            protocol::CallId(id),
            protocol::QuestionAnswer {
                question: id,
                responses: BTreeMap::from([(key.clone(), protocol::Response::accept())]),
            },
        ),
    )
    .unwrap();
    assert_eq!(waiter.join().unwrap(), (json!({"ok":[]}), 0));
    assert_eq!(
        std::fs::read_to_string(engine.root.dir.join("endpoint.tokens")).unwrap(),
        "1"
    );
    engine.stop();
}

#[test]
fn declining_after_the_browser_callback_discards_the_authorization_code() {
    use protocol::{Body, ClientMsg, DaemonMsg, read_frame, write_frame};
    let mut engine = Engine::new("rotating");
    let mut client = Client::attach(&engine.root.dir);
    write_frame(
        &mut client.writer,
        &ClientMsg::Input {
            segments: "call mcp_list {\"server\":\"fixture\"}".into(),
            mode: protocol::Mode::default(),
            force: false,
            client_seq: None,
        },
    )
    .unwrap();
    let (id, question) = loop {
        if let Some(DaemonMsg::Entry(entry)) = read_frame::<DaemonMsg>(&mut client.reader).unwrap()
            && let Body::Question { question } = entry.body
        {
            break (entry.seq, question);
        }
    };
    let (key, protocol::Prompt::Url { url, .. }) = question.prompts.iter().next().unwrap() else {
        panic!("URL question");
    };
    let response = get(url);
    let redirect = response
        .lines()
        .find_map(|line| line.strip_prefix("Location: "))
        .unwrap();
    assert!(get(redirect).contains("Return to a"));
    assert!(!engine.root.dir.join("endpoint.tokens").exists());
    write_frame(
        &mut client.writer,
        &ClientMsg::answer(
            protocol::CallId(id),
            protocol::QuestionAnswer {
                question: id,
                responses: BTreeMap::from([(key.clone(), protocol::Response::Decline)]),
            },
        ),
    )
    .unwrap();
    loop {
        if let Some(DaemonMsg::Entry(entry)) = read_frame::<DaemonMsg>(&mut client.reader).unwrap()
            && let Body::ToolResult {
                outcome:
                    protocol::Outcome::Done {
                        output: protocol::Payload::Inline(output),
                    },
                ..
            } = entry.body
        {
            assert!(
                output
                    .parse::<serde_json::Value>()
                    .unwrap()
                    .get("failed")
                    .is_some()
            );
            break;
        }
    }
    assert!(!engine.root.dir.join("endpoint.tokens").exists());
    engine.stop();
}
