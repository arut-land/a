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
        stream(move |mut updates| async move {
            for _ in 0..said.times {
                updates.send(said.text.clone()).await;
            }
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

#[test]
fn a_blocked_query_sink_suspends_the_producer_until_delivery() {
    use std::future::poll_fn;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    struct Gate {
        open: AtomicBool,
        delivered: Mutex<Vec<usize>>,
    }
    impl Sink for Gate {
        fn send(&self, update: Json) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                poll_fn(|_| {
                    if self.open.load(Ordering::Relaxed) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                self.delivered.lock().unwrap().push(update.parse().unwrap());
            })
        }
    }

    let produced = AtomicUsize::new(0);
    let produced = &produced;
    let streamed = stream(|mut updates| async move {
        for i in 0..1_000 {
            produced.fetch_add(1, Ordering::Relaxed);
            updates.send(i).await;
        }
        Ok::<_, String>("done")
    });
    let sink = Gate {
        open: AtomicBool::new(false),
        delivered: Mutex::default(),
    };
    let mut answer = Answered::run(streamed, &sink);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(answer.as_mut().poll(&mut cx).is_pending());
    assert_eq!(produced.load(Ordering::Relaxed), 1);
    assert!(sink.delivered.lock().unwrap().is_empty());
    sink.open.store(true, Ordering::Relaxed);
    assert_eq!(rt::block_on(answer), Ok("done"));
    assert_eq!(
        *sink.delivered.lock().unwrap(),
        (0..1_000).collect::<Vec<_>>()
    );
}
