use std::path::Path;
use std::sync::Mutex;

use protocol::{Json, Outcome, SessionId};
use serde::Deserialize;

use super::*;
use crate::host::{Host, Parts, Registry};
use crate::{Cancel, Cx, Extension, Registrar, States, Tool};

/// An input that counts how often it is read.
#[derive(Debug)]
struct Counted(u64);

thread_local! {
    /// The reads on this thread: each test has its own.
    static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl<'de> Deserialize<'de> for Counted {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        READS.set(READS.get() + 1);
        u64::deserialize(d).map(Self)
    }
}

impl crate::Schema for Counted {
    fn schema(out: &mut String) {
        out.push_str(r#"{"type":"integer"}"#);
    }
}

/// Done at once.
struct Double;

impl Tool for Double {
    type In = Counted;
    type Out = u64;
    const NAME: &'static str = "double";
    const DESCRIPTION: &'static str = "Doubles.";

    fn call(&self, input: &Counted, _: &Cx) -> Result<u64, String> {
        Ok(input.0 * 2)
    }
}

/// Waits.
struct Later;

impl Tool for Later {
    type In = Counted;
    type Out = u64;
    const NAME: &'static str = "later";
    const DESCRIPTION: &'static str = "Waits, then answers.";

    async fn call(&self, input: &Counted, _: &Cx) -> Result<u64, String> {
        std::future::ready(()).await;
        Ok(input.0 + 1)
    }
}

/// Counts to its input, each number a partial, and ends with the sum.
struct Count;

impl Tool for Count {
    type In = Counted;
    type Out = u64;
    const NAME: &'static str = "count";
    const DESCRIPTION: &'static str = "Counts.";

    fn call(
        &self,
        input: &Counted,
        _: &Cx,
    ) -> Streamed<String, impl Future<Output = Result<u64, String>> + Send, u64> {
        let n = input.0;
        stream(move |partials: Partials<String>| async move {
            for i in 1..=n {
                partials.send(format!("{i} "));
                std::future::ready(()).await;
            }
            Ok((1..=n).sum())
        })
    }
}

struct Tools;

impl Extension for Tools {
    const NAME: &'static str = "tools";
    const VERSION: u32 = 1;

    fn contribute(&self, r: &mut Registrar) {
        r.tool(Double).tool(Later).tool(Count);
    }
}

/// The engine as these calls reach it: their progress, kept.
struct Recorded {
    registry: Registry,
    progress: Mutex<String>,
}

impl Host for Recorded {
    fn spawned(&self, _: u32, _: bool) {}
    fn own(&self, _: u32, _: BoxFuture<'static, ()>) {}
    fn progress(&self, text: &str) {
        self.progress.lock().unwrap().push_str(text);
    }
    fn note(&self, _: &str, _: &str, _: Json) -> BoxFuture<'static, ()> {
        Box::pin(std::future::ready(()))
    }
    fn edit(&self, _: protocol::Seq, _: Option<protocol::Seq>, _: Option<Json>) {}
    fn states(&self, read: &mut dyn FnMut(&States)) {
        read(&States::default());
    }
    fn registry(&self) -> &Registry {
        &self.registry
    }
}

fn run(tool: &str, input: u64) -> (Outcome, String) {
    let host = Arc::new(Recorded {
        registry: Registry::default().with(&Tools).unwrap(),
        progress: Mutex::default(),
    });
    let cx = Parts {
        workspace: Path::new("/w").into(),
        env: None,
        session: SessionId::new(),
        call: protocol::CallId::new(1).unwrap(),
        model: "test:model".into(),
        spill: Path::new("/s").into(),
        cancel: Cancel::default(),
        services: Arc::default(),
    }
    .cx(Arc::clone(&host) as _);
    let input = Json::encode(&input).unwrap();
    let called = rt::block_on(host.registry.tools().run(tool, &input, None, &cx));
    let progress = host.progress.lock().unwrap().clone();
    (called.outcome, progress)
}

fn done(n: u64) -> Outcome {
    Outcome::Done {
        output: Json::encode(&n).unwrap(),
    }
}

/// Nothing says what shape a call is but what its tool returns: a value,
/// a future, a stream, each run by the same toolset.
#[test]
fn the_call_shape_follows_the_return_type() {
    assert_eq!(run("double", 4), (done(8), String::new()));
    assert_eq!(run("later", 4), (done(5), String::new()));
    assert_eq!(run("count", 3), (done(6), "1 2 3 ".to_owned()));
}

/// A streaming call yields each partial in order, then its output, and
/// nothing after; a call's partials are its progress.
#[test]
fn a_streaming_tool_reports_partials_then_its_result() {
    let mut streamed = stream(|partials: Partials<&str>| async move {
        partials.send("a");
        std::future::ready(()).await;
        partials.send("b");
        // Sent as it ends: still before the output.
        partials.send("c");
        Ok::<_, String>(3)
    });
    let mut got = Vec::new();
    loop {
        match rt::block_on(streamed.next()) {
            Next::Partial(p) => got.push(p),
            Next::Done(output) => {
                assert_eq!(output, Ok(3));
                break;
            }
        }
    }
    assert_eq!(got, ["a", "b", "c"]);
    assert_eq!(run("count", 2).1, "1 2 ");
}

/// A call's input is read once, from its text, into the tool's type: for
/// the run and its cards alike, given what was read.
#[test]
fn a_tool_input_is_read_once_per_call() {
    let registry = Registry::default().with(&Tools).unwrap();
    let tool = registry.tools().get("double").unwrap();
    let parsed = tool.parse(&Json::encode(&2).unwrap()).unwrap();
    let _ = tool.present(&parsed);
    assert!(parsed.get::<Counted>().is_some());
    assert_eq!(READS.get(), 1);
}
