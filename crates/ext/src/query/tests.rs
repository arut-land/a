use std::future::Future;
use std::sync::Mutex;

use protocol::MachineId;
use serde::Deserialize;

use super::*;
use crate::{Extension, Registrar, Streamed, stream};

/// Says `text` back, `times` times as updates first: about a workspace.
struct Echo;

#[derive(Deserialize)]
struct Said {
    text: String,
    times: u32,
}

impl Query for Echo {
    const NAME: &'static str = "echo";
    const DESCRIPTION: &'static str = "says it back";
    type Scope = Workspace;
    type Request = Said;
    type Reply = String;

    fn answer(
        &self,
        said: Said,
        cx: &QueryCx<Workspace>,
    ) -> Streamed<String, impl Future<Output = Result<String, String>> + Send, String> {
        let path = cx.scope.path.display().to_string();
        stream(move |updates| async move {
            (0..said.times).for_each(|_| updates.send(said.text.clone()));
            Ok(format!("{} in {path}", said.text))
        })
    }
}

/// Answers at once, about the daemon: its shape is its return type.
struct Ping;

impl Query for Ping {
    const NAME: &'static str = "ping";
    const DESCRIPTION: &'static str = "pong";
    type Request = ();
    type Reply = &'static str;

    fn answer(&self, (): (), _: &QueryCx<Daemon>) -> Result<&'static str, String> {
        Ok("pong")
    }
}

struct Queries;

impl Extension for Queries {
    const NAME: &'static str = "queries";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.query(Echo).query(Ping);
    }
}

/// Keeps every update, as its text.
#[derive(Default)]
struct Kept(Mutex<Vec<String>>);

impl Sink for Kept {
    fn send(&self, update: Json) -> BoxFuture<'_, ()> {
        self.0.lock().unwrap().push(update.to_string());
        Box::pin(std::future::ready(()))
    }
}

fn ask(name: &str, body: &str, at: &At) -> (Vec<String>, Result<Json, String>) {
    let registry = Arc::new(Registry::default().with(&Queries).unwrap());
    let query = registry.query(name).unwrap();
    let updates = Kept::default();
    let asked = Asked {
        machine: MachineId([0; 16]),
        role: Role::Viewer,
        cancel: Cancel::default(),
        services: Arc::default(),
        registry: Arc::clone(&registry),
    };
    let body = serde_json::from_str::<Json>(body).unwrap();
    let reply = rt::block_on(query.answer(&body, at, asked, &updates));
    (updates.0.into_inner().unwrap(), reply)
}

#[test]
fn a_streamed_answer_sends_its_partials_as_updates_before_its_reply() {
    let at = At::Workspace(Arc::from(Path::new("/w")));
    let (updates, reply) = ask("echo", r#"{"text":"hi","times":2}"#, &at);
    assert_eq!(updates, [r#""hi""#, r#""hi""#]);
    assert_eq!(reply.unwrap().to_string(), r#""hi in /w""#);
    let (updates, reply) = ask("ping", "null", &At::Daemon);
    assert_eq!(updates, Vec::<String>::new());
    assert_eq!(reply.unwrap().to_string(), r#""pong""#);
}

#[test]
fn a_query_is_answered_only_about_a_scope_that_implies_its_own() {
    let body = r#"{"text":"hi","times":0}"#;
    let (_, reply) = ask("echo", body, &At::Daemon);
    assert_eq!(reply.unwrap_err(), "echo answers about a workspace");
    let session = At::Session(SessionId::default(), Arc::from(Path::new("/s")));
    let (_, reply) = ask("echo", body, &session);
    assert_eq!(reply.unwrap().to_string(), r#""hi in /s""#);
    let (_, reply) = ask("ping", "null", &session);
    assert!(reply.is_ok());
}

#[test]
fn a_request_is_read_once_into_its_type() {
    let at = At::Workspace(Arc::from(Path::new("/w")));
    let (_, reply) = ask("echo", r#"{"text":1}"#, &at);
    assert!(reply.unwrap_err().starts_with("invalid request"));
}
