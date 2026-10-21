use super::*;
use ext::Concurrency;

struct Owner<const SHARED: bool>;
impl<const SHARED: bool> Tool for Owner<SHARED> {
    type In = ext::Args;
    type Out = Outcome;
    const NAME: &'static str = if SHARED { "shared_owner" } else { "owner" };
    const DESCRIPTION: &'static str = "invoke owned work";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: Concurrency = if SHARED {
        Concurrency::Shared
    } else {
        Concurrency::Exclusive
    };
    async fn call(&self, args: &ext::Args, cx: &Cx) -> Result<Outcome, String> {
        let input = Json::encode(args).unwrap();
        let tool = match args.args.as_str() {
            "safe" => "safe_effect",
            "nested" => "branch",
            _ => "effect",
        };
        let first = cx.invoke("first", tool, &input);
        if matches!(args.args.as_str(), "parallel" | "nested") {
            let mut first = pin!(first);
            let mut second = pin!(cx.invoke("second", tool, &input));
            let (mut a, mut b) = (None, None);
            poll_fn(|cx| {
                if a.is_none()
                    && let Poll::Ready(result) = first.as_mut().poll(cx)
                {
                    a = Some(result);
                }
                if b.is_none()
                    && let Poll::Ready(result) = second.as_mut().poll(cx)
                {
                    b = Some(result);
                }
                if a.is_some() && b.is_some() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            let cached = cx.invoke("first", tool, &input).await;
            assert_eq!(a.unwrap(), cached);
            Ok(b.unwrap().outcome)
        } else {
            Ok(first.await.outcome)
        }
    }
}

#[derive(Clone)]
struct Effect<const SAFE: bool> {
    calls: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    release: Flag,
}
impl<const SAFE: bool> Tool for Effect<SAFE> {
    type In = ext::Args;
    type Out = bool;
    const NAME: &'static str = if SAFE { "safe_effect" } else { "effect" };
    const DESCRIPTION: &'static str = "held owned effect";
    const REPLAY: Replay = if SAFE { Replay::Safe } else { Replay::Unsafe };
    const CONCURRENCY: Concurrency = if SAFE {
        Concurrency::Shared
    } else {
        Concurrency::Exclusive
    };
    async fn call(&self, _: &ext::Args, cx: &Cx) -> Result<bool, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        or(self.release.wait(), cx.cancel.wait()).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(!cx.cancel.is_set())
    }
}
impl<const SAFE: bool> Extension for Effect<SAFE> {
    const NAME: &'static str = "invocation";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(self.clone());
        r.tool(Owner::<false>);
        r.tool(Owner::<true>);
        r.on::<point::Permission>(|_, tool| {
            let denied =
                tool.tool == "effect" && tool.input.parse::<Value>().unwrap()["args"] == "denied";
            if denied {
                Permission::Deny
            } else {
                Permission::Allow
            }
        });
    }
}

#[test]
fn delegation_obeys_the_callees_replay_and_concurrency_contract() {
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Flag::default();
    let maximum = Arc::new(AtomicUsize::new(0));
    let effect = Effect::<false> {
        calls: calls.clone(),
        release: release.clone(),
        maximum: maximum.clone(),
        active: Arc::default(),
    };
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default().with(&effect).unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session
        .submit("call owner {\"args\":\"parallel\"}")
        .unwrap();
    session.wait("first invocation started", PATIENCE, || {
        calls.load(Ordering::SeqCst) == 1
    });
    session.barrier();
    let prefix = log_of(&session);
    assert!(
        !prefix
            .iter()
            .any(|entry| matches!(entry.body, Body::ToolResult { .. }))
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exclusive siblings cannot overlap"
    );
    release.set();
    settle(&session);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
    let log = log_of(&session);
    let root = log.iter().find(|entry| matches!(&entry.body, Body::ToolCall { invoke: None, tool, .. } if tool == "owner")).unwrap().seq;
    let children: Vec<_> = log
        .iter()
        .filter_map(|entry| match &entry.body {
            Body::ToolCall {
                invoke: Some(key), ..
            } => {
                assert_eq!(key.parent.0, root);
                Some(entry.seq)
            }
            _ => None,
        })
        .collect();
    assert_eq!(children.len(), 2);
    let root_result = log
        .iter()
        .position(|entry| matches!(entry.body, Body::ToolResult { call, .. } if call.0 == root))
        .unwrap();
    let replay = fixture.start(&log[..root_result]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    replay.halt();
    for child in children {
        assert!(
            log.iter()
                .position(
                    |entry| matches!(entry.body, Body::ToolResult { call, .. } if call.0 == child)
                )
                .unwrap()
                < root_result
        );
    }
    session.submit("call shared_owner {}").unwrap();
    settle(&session);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    session.submit("call owner {\"args\":\"denied\"}").unwrap();
    settle(&session);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let log = log_of(&session);
    let denied = log
        .iter()
        .rev()
        .find_map(|entry| match &entry.body {
            Body::ToolCall {
                invoke: Some(_),
                input,
                ..
            } if input.parse::<Value>().unwrap()["args"] == "denied" => Some(CallId(entry.seq)),
            _ => None,
        })
        .unwrap();
    assert!(log.iter().any(|entry| matches!(entry.body, Body::Decision { call: Some(call), d: Verdict::Permission { permission: Permission::Deny }, .. } if call == denied)));
    session.halt();
    recover::<false>();
    recover::<true>();
}

fn recover<const SAFE: bool>() {
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Flag::default();
    let effect = Effect::<SAFE> {
        calls: calls.clone(),
        release: release.clone(),
        maximum: Arc::default(),
        active: Arc::default(),
    };
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default().with(&effect).unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session
        .submit(if SAFE {
            "call owner {\"args\":\"safe\"}"
        } else {
            "call owner {}"
        })
        .unwrap();
    session.wait("callee started", PATIENCE, || {
        calls.load(Ordering::SeqCst) == 1
    });
    session.barrier();
    let prefix = log_of(&session);
    let child = prefix
        .iter()
        .find(|entry| {
            matches!(
                entry.body,
                Body::ToolCall {
                    invoke: Some(_),
                    ..
                }
            )
        })
        .unwrap()
        .seq;
    session.halt();
    let recovered = fixture.load(&prefix);
    if SAFE {
        recovered.wait("safe callee resumed", PATIENCE, || {
            calls.load(Ordering::SeqCst) == 2
        });
        release.set();
    }
    settle(&recovered);
    assert_eq!(calls.load(Ordering::SeqCst), if SAFE { 2 } else { 1 });
    let log = log_of(&recovered);
    assert_eq!(
        log.iter()
            .filter(|entry| matches!(
                entry.body,
                Body::ToolCall {
                    invoke: Some(_),
                    ..
                }
            ))
            .count(),
        1,
        "recovery retains the original intent"
    );
    assert!(log.iter().any(|entry| matches!(&entry.body, Body::ToolResult { call, outcome, .. } if call.0 == child && (SAFE || matches!(outcome, Outcome::Interrupted { .. })))));
    recovered.halt();
}

#[derive(Clone)]
struct Branch(Arc<(AtomicUsize, rt::Notify)>);
impl Tool for Branch {
    type In = ext::Args;
    type Out = bool;
    const NAME: &'static str = "branch";
    const DESCRIPTION: &'static str = "await an exclusive descendant";
    const REPLAY: Replay = Replay::Safe;
    const CONCURRENCY: Concurrency = Concurrency::Shared;
    async fn call(&self, _: &ext::Args, cx: &Cx) -> Result<bool, String> {
        self.0.0.fetch_add(1, Ordering::SeqCst);
        self.0.1.notify();
        or(
            self.0.1.until(|| self.0.0.load(Ordering::SeqCst) == 2),
            cx.cancel.wait(),
        )
        .await;
        match cx
            .invoke(
                "exclusive",
                "effect",
                &Json::encode(&ext::Args {
                    args: String::new(),
                })
                .unwrap(),
            )
            .await
            .outcome
        {
            Outcome::Failed { error } => Err(error),
            _ => Ok(true),
        }
    }
}
impl Extension for Branch {
    const NAME: &'static str = "branch";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(self.clone());
    }
}

#[test]
fn shared_siblings_cannot_deadlock_on_exclusive_descendants() {
    let entered = Arc::new((AtomicUsize::new(0), rt::Notify::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let effect = Effect::<false> {
        calls: calls.clone(),
        active: Arc::default(),
        maximum: Arc::default(),
        release: Flag::default(),
    };
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default()
            .with(&effect)
            .unwrap()
            .with(&Branch(entered.clone()))
            .unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session.submit("call owner {\"args\":\"nested\"}").unwrap();
    settle(&session);
    assert_eq!(entered.0.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let log = log_of(&session);
    assert_eq!(log.iter().filter(|e| matches!(&e.body, Body::ToolResult { outcome: Outcome::Failed { error }, .. } if error == "exclusive invocation requires exclusive ancestors")).count(), 2);
    session.halt();
}

struct InvocationProbe;
impl Tool for InvocationProbe {
    type In = ext::Args;
    type Out = usize;
    const NAME: &'static str = "invocation_probe";
    const DESCRIPTION: &'static str = "inspect owned invocation state";
    const REPLAY: Replay = Replay::Safe;
    async fn call(&self, args: &ext::Args, cx: &Cx) -> Result<usize, String> {
        let count = cx.invocation().local(0, || Arc::new(AtomicUsize::new(0)));
        let current = count.fetch_add(1, Ordering::SeqCst) + 1;
        if args.args == "leaf" {
            return Ok(current);
        }
        match cx
            .invoke(
                "leaf",
                <Self as Tool>::NAME,
                &Json::encode(&ext::Args {
                    args: "leaf".into(),
                })
                .unwrap(),
            )
            .await
            .outcome
        {
            Outcome::Done { output } => output.parse().map_err(|e| e.to_string()),
            outcome => Err(format!("unexpected invocation result: {outcome:?}")),
        }
    }
}
impl Extension for InvocationProbe {
    const NAME: &'static str = "invocation_probe";
    const VERSION: u32 = 1;
    fn contribute(&self, r: &mut Registrar) {
        r.tool(Self);
    }
}

#[test]
fn ordered_callees_inherit_context_before_the_durable_acknowledgement() {
    let fixture = Fixture::new(
        ext_fixtures::ECHO,
        Registry::default().with(&InvocationProbe).unwrap(),
        openai(&Script::new(&[])),
    );
    let session = fixture.create();
    session.submit("call invocation_probe {}").unwrap();
    settle(&session);
    let log = log_of(&session);
    let root = log
        .iter()
        .find(|entry| matches!(entry.body, Body::ToolCall { invoke: None, .. }))
        .unwrap()
        .seq;
    assert!(log.iter().any(|entry| matches!(&entry.body, Body::ToolResult { call, outcome: Outcome::Done { output }, .. } if call.0 == root && output.parse::<usize>().unwrap() == 2)));
    session.halt();
}
