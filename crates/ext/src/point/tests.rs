use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use protocol::Permission as Level;
use serde_json::{Value, json};

use super::*;
use crate::{Registrar, SpawnSpec};

/// Counts every time a handler's context is read.
#[derive(Default)]
struct Counted(AtomicUsize);

impl View for Counted {
    fn model(&self) -> String {
        self.0.fetch_add(1, Ordering::SeqCst);
        "test:model".to_owned()
    }

    fn states(&self, read: &mut dyn FnMut(&States)) {
        self.0.fetch_add(1, Ordering::SeqCst);
        read(&States::default());
    }
}

fn point_cx(view: &Counted) -> PointCx<'_> {
    PointCx::new(SessionId::new(), Path::new("/w"), view)
}

/// The hooks extensions named as given register.
fn hooks(by: Vec<(&str, Registrar)>) -> Hooks {
    let mut hooks = Hooks::default();
    for (name, made) in by {
        for hooked in &made.hooks {
            hooks.add(&Name::from(name), hooked);
        }
    }
    hooks
}

fn one(name: &str, register: impl FnOnce(&mut Registrar)) -> (&str, Registrar) {
    let mut r = Registrar::default();
    register(&mut r);
    (name, r)
}

fn names(decided: &Decided) -> Vec<&str> {
    decided.iter().map(|(name, _)| &**name).collect()
}

/// Every point, by type.
macro_rules! every_point {
    ($m:ident) => {
        $m!(
            Input,
            RequestError,
            PreTool,
            Permission,
            Approve,
            Spawn,
            PostTool,
            OnStop,
            Observe,
            SessionStart,
            SessionEnd,
            Turn,
            Reply,
            Resume,
            Step
        )
    };
}

/// The design's merge table ("Hooks intercept only at decision points").
const TABLE: &[(&str, Merge)] = &[
    ("input", Merge::Chain),
    ("request_error", Merge::FirstSome),
    ("pre_tool", Merge::Chain),
    ("permission", Merge::MostRestrictive),
    ("approve", Merge::FirstSome),
    ("spawn", Merge::Chain),
    ("post_tool", Merge::Chain),
    ("on_stop", Merge::FirstSome),
    ("observe", Merge::Observe),
    ("session_start", Merge::Observe),
    ("session_end", Merge::Observe),
    ("turn", Merge::Chain),
    ("reply", Merge::Chain),
    ("resume", Merge::FirstSome),
    ("step", Merge::Chain),
];

/// A chain point merges as a chain: each handler sees what the last left,
/// every replacement is reported with what it replaced, and the first stop
/// ends it.
fn chains<P, T, S>(cx: &P::Cx<'_>, facts: P::Facts<'_>, [start, a, b]: [T; 3], stop: Option<S>)
where
    P: Point<Out = Rewrite<T, S>>,
    T: Clone + PartialEq + Debug + Send + Sync + 'static,
    S: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let (ra, rb) = (a.clone(), b.clone());
    let set = hooks(vec![
        one("pass", |r| {
            r.on::<P>(|_, _| Rewrite::Pass);
        }),
        one("a", move |r| {
            r.on::<P>(move |_, _| Rewrite::Replace(ra.clone()));
        }),
        one("b", move |r| {
            r.on::<P>(move |_, _| Rewrite::Replace(rb.clone()));
        }),
    ]);
    let mut subject = start.clone();
    let mut replaced = Vec::new();
    let merged = set.run::<P>(cx, facts, &mut subject, &mut |name, before, after| {
        replaced.push((name.to_string(), before, after.clone()));
    });
    assert!(merged.is_ok(), "{}", P::NAME);
    assert_eq!(subject, b, "{}", P::NAME);
    let want = [
        ("a".to_owned(), start.clone(), a.clone()),
        ("b".to_owned(), a.clone(), b.clone()),
    ];
    assert_eq!(replaced, want, "{}", P::NAME);

    let Some(stop) = stop else { return };
    let (ra, rb, why) = (a.clone(), b.clone(), stop.clone());
    let asked = Arc::new(AtomicUsize::new(0));
    let after = Arc::clone(&asked);
    let set = hooks(vec![
        one("a", move |r| {
            r.on::<P>(move |_, _| Rewrite::Replace(ra.clone()));
        }),
        one("stop", move |r| {
            r.on::<P>(move |_, _| Rewrite::Stop(why.clone()));
        }),
        one("b", move |r| {
            r.on::<P>(move |_, _| {
                after.fetch_add(1, Ordering::SeqCst);
                Rewrite::Replace(rb.clone())
            });
        }),
    ]);
    let mut subject = start;
    let merged = set.run::<P>(cx, facts, &mut subject, &mut |_, _, _| {});
    let Err((who, why)) = merged else {
        panic!("{} did not stop", P::NAME)
    };
    assert_eq!((&*who, why, subject), ("stop", stop, a), "{}", P::NAME);
    assert_eq!(asked.load(Ordering::SeqCst), 0, "{}", P::NAME);
}

/// A first-answer point takes the first answer, and asks no one after.
fn first<P, T>(cx: &P::Cx<'_>, facts: P::Facts<'_>, [a, b]: [T; 2])
where
    P: Point<Out = Option<T>>,
    T: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let asked = Arc::new(AtomicUsize::new(0));
    let after = Arc::clone(&asked);
    let set = hooks(vec![
        one("none", |r| {
            r.on::<P>(|_, _| None);
        }),
        one("a", move |r| {
            r.on::<P>(move |_, _| Some(a.clone()));
        }),
        one("b", move |r| {
            r.on::<P>(move |_, _| {
                after.fetch_add(1, Ordering::SeqCst);
                Some(b.clone())
            });
        }),
    ]);
    let merged = set.run::<P>(cx, facts, &mut (), &mut |_, (), ()| {});
    assert_eq!(
        merged.map(|(name, _)| name.to_string()),
        Some("a".into()),
        "{}",
        P::NAME
    );
    assert_eq!(asked.load(Ordering::SeqCst), 0, "{}", P::NAME);
}

/// A first-answer point whose handlers may answer later, all answering
/// now, merges as one that may not.
fn first_now<P, T>(cx: &P::Cx<'_>, facts: P::Facts<'_>, [a, b]: [T; 2])
where
    P: Point<Out = Answer<Option<T>>>,
    T: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let asked = Arc::new(AtomicUsize::new(0));
    let after = Arc::clone(&asked);
    let set = hooks(vec![
        one("none", |r| {
            r.on::<P>(|_, _| Answer::Now(None));
        }),
        one("a", move |r| {
            r.on::<P>(move |_, _| Answer::Now(Some(a.clone())));
        }),
        one("b", move |r| {
            r.on::<P>(move |_, _| {
                after.fetch_add(1, Ordering::SeqCst);
                Answer::Now(Some(b.clone()))
            });
        }),
    ]);
    let merged = set.run::<P>(cx, facts, &mut (), &mut |_, (), ()| {});
    assert!(
        matches!(&merged, Asked::Now(name, _) if &**name == "a"),
        "{}: {merged:?}",
        P::NAME
    );
    assert_eq!(asked.load(Ordering::SeqCst), 0, "{}", P::NAME);
}

/// An observed point tells every handler, in order.
fn observes<P: Point<Out = ()>>(cx: &P::Cx<'_>, facts: P::Facts<'_>) {
    let told = Arc::new(Mutex::new(Vec::new()));
    let by = ["a", "b", "c"].map(|name| {
        let told = Arc::clone(&told);
        one(name, move |r| {
            r.on::<P>(move |_, _| told.lock().unwrap().push(name));
        })
    });
    hooks(by.into()).run::<P>(cx, facts, &mut (), &mut |_, (), ()| {});
    assert_eq!(*told.lock().unwrap(), ["a", "b", "c"], "{}", P::NAME);
}

/// Every point says what a failing handler comes to, and what decides
/// about running something fails closed.
#[test]
fn every_point_declares_its_failure_policy() {
    use OnError::{FailClosed, FailOpen};
    macro_rules! table {
        ($($point:ident),*) => { [$(($point::NAME, $point::ON_ERROR)),*] };
    }
    let declared = every_point!(table);
    let want = [
        ("input", FailClosed),
        ("request_error", FailOpen),
        ("pre_tool", FailClosed),
        ("permission", FailClosed),
        ("approve", FailClosed),
        ("spawn", FailClosed),
        ("post_tool", FailOpen),
        ("on_stop", FailOpen),
        ("observe", FailOpen),
        ("session_start", FailOpen),
        ("session_end", FailOpen),
        ("turn", FailOpen),
        ("reply", FailOpen),
        ("resume", FailOpen),
        ("step", FailOpen),
    ];
    assert_eq!(declared, want);
}

#[test]
fn merge_rules_are_applied_identically_for_every_point() {
    use Level::{Allow, Ask, Defer, Deny};

    macro_rules! table {
        ($($point:ident),*) => { [$(($point::NAME, $point::MERGE)),*] };
    }
    assert_eq!(every_point!(table), TABLE);

    let request = |model: &str| llm::Request {
        model: model.into(),
        ..llm::Request::default()
    };
    let spawn = |program: &str| SpawnSpec {
        argv: vec![program.into()],
        env: BTreeMap::new(),
        cwd: "/".into(),
    };
    let failed = |error: &str| Outcome::Failed {
        error: error.into(),
    };
    let input = Json::new(&json!({}));
    let call = ToolUse {
        tool: "t",
        input: &input,
    };
    let texts = ["0", "a", "b"].map(str::to_owned);
    let stop = Some("no".to_owned());
    let view = Counted::default();
    let cx = &point_cx(&view);
    chains::<Input, _, _>(cx, (), texts, stop.clone());
    chains::<PreTool, _, _>(cx, "t", [0, 1, 2].map(|n| Json::new(&json!(n))), stop);
    chains::<Spawn, _, _>(cx, (), ["0", "a", "b"].map(spawn), None);
    chains::<PostTool, _, _>(cx, call, ["0", "a", "b"].map(failed), None);
    let say = |text: &str| Actions::from_elem(crate::Action::Say(text.into()), 1);
    let user = crate::Input::User { text: "hi".into() };
    chains::<Turn, _, _>(&(), &user, ["0", "a", "b"].map(say), None);
    let reply = llm::Response {
        message: llm::Assistant {
            content: Vec::new(),
            origin: None,
        },
        finish: llm::Finish::new(llm::Reason::Stop, None),
        usage: llm::Usage::default(),
        id: None,
        model: None,
        meta: llm::Meta::new(),
    };
    chains::<Reply, _, _>(&(), (1, &reply), ["0", "a", "b"].map(say), None);
    first::<Resume, _>(&(), 1, [crate::Resume::Final, crate::Resume::Stop]);

    let overflow = llm::Error::new(llm::Kind::ContextOverflow, "overflow");
    let error = Failed {
        error: &overflow,
        attempt: 0,
    };
    let after = |ms| Recover::Retry {
        after: Duration::from_millis(ms),
    };
    first_now::<RequestError, _>(cx, error, [after(1), Recover::Resend]);
    first_now::<Approve, _>(cx, call, [true, false]);
    chains::<Step, _, _>(cx, (), ["0", "a", "b"].map(request), None);
    first::<OnStop, _>(cx, (), ["a", "b"].map(str::to_owned));

    let entry = Entry::new(1, 0, protocol::Body::INTERRUPT);
    observes::<Observe>(cx, &entry);
    observes::<SessionStart>(cx, ());
    observes::<SessionEnd>(cx, ());

    // The one most-restrictive point: the strictest wins, the earliest of
    // equals, and all allowing is no answer.
    let cases = [
        (vec![Allow, Allow], None),
        (vec![Ask, Allow], Some(("a", Ask))),
        (vec![Ask, Defer, Ask], Some(("b", Defer))),
        (vec![Defer, Deny, Ask], Some(("b", Deny))),
        (vec![Deny, Deny], Some(("a", Deny))),
    ];
    let view = Counted::default();
    for (levels, want) in cases {
        let by = ["a", "b", "c"]
            .into_iter()
            .zip(levels.clone())
            .map(|(name, level)| {
                one(name, move |r| {
                    r.on::<Permission>(move |_, _| level);
                })
            })
            .collect();
        let got = hooks(by).run::<Permission>(&point_cx(&view), call, &mut (), &mut |_, (), ()| {});
        let got = got.map(|(name, level)| (name.to_string(), level));
        assert_eq!(got, want.map(|(n, l)| (n.to_owned(), l)), "{levels:?}");
    }
}

#[test]
fn a_point_with_no_handler_costs_nothing() {
    let view = Counted::default();
    let cx = point_cx(&view);
    let input = Json::new(&json!({ "n": 0 }));
    // Handlers elsewhere are not asked, nor is the context read.
    let elsewhere = hooks(vec![one("other", |r| {
        r.on::<OnStop>(|cx, ()| Some(cx.model()));
    })]);
    for hooks in [Hooks::default(), elsewhere] {
        let outcome = Outcome::Interrupted { output: None };
        let text = hooks.input(&cx, "hi".to_owned());
        assert_eq!(text, Ok(Chained::new("hi".to_owned())));
        let subject = input.clone();
        assert_eq!(
            hooks.gate(&cx, "t", &subject),
            Gating::Now(Gated::run(subject))
        );
        let error = llm::Error::new(llm::Kind::Server, "e");
        let failed = Failed {
            error: &error,
            attempt: 0,
        };
        assert_eq!(hooks.request_error(&cx, failed), Asked::None);
        let mut request = llm::Request::default();
        assert_eq!(hooks.step(&cx, &mut request), (Decided::new(), None));
        let after = hooks.post_tool(&cx, ("t", &input), outcome.clone());
        assert_eq!(after, Chained::new(outcome));
        hooks.observe(&cx, &Entry::new(1, 0, protocol::Body::INTERRUPT));
        hooks.session_start(&cx);
        hooks.session_end(&cx);
    }
    assert_eq!(view.0.load(Ordering::SeqCst), 0);
    // A handler that reads its context reads it then.
    let reads = hooks(vec![one("reader", |r| {
        r.on::<OnStop>(|cx, ()| Some(cx.model()));
    })]);
    let said = reads.on_stop(&cx);
    assert_eq!(said.map(|(_, text)| text).as_deref(), Some("test:model"));
    assert_eq!(view.0.load(Ordering::SeqCst), 1);
}

#[test]
fn gate_logs_what_the_chain_and_the_policy_decided() {
    let view = Counted::default();
    let cx = point_cx(&view);
    let bump = |r: &mut Registrar| {
        r.on::<PreTool>(|_, call| {
            let n = call.input.value()["n"].as_i64().unwrap();
            Rewrite::Replace(Json::new(&json!({ "n": n + 1 })))
        });
    };
    let gate = |hooks: &Hooks| {
        let gated = (hooks.gate(&cx, "t", &Json::new(&json!({ "n": 0 }))).now()).unwrap();
        (gated.gate, gated.input.value(), gated.decisions)
    };
    let (verdict, input, decided) = gate(&hooks(vec![one("a", bump), one("b", bump)]));
    assert_eq!((verdict, input), (Gate::Run, json!({ "n": 2 })));
    assert_eq!(names(&decided), ["a", "b"]);
    assert!(
        matches!(&decided[1].1, Verdict::PreTool { input } if input.value() == json!({ "n": 2 }))
    );

    let blocked = hooks(vec![
        one("a", bump),
        one("b", |r| {
            r.on::<PreTool>(|_, _| Rewrite::Stop("no".into()));
        }),
    ]);
    let (verdict, _, decided) = gate(&blocked);
    assert_eq!(verdict, Gate::Refuse("no".into()));
    assert!(
        matches!(&decided[..], [(_, Verdict::PreTool { .. }), (b, Verdict::Block { .. })] if &**b == "b")
    );

    let asks = |approve: Option<bool>| {
        hooks(vec![
            one("policy", |r| {
                r.on::<Permission>(|_, _| Level::Ask);
            }),
            one("approver", move |r| {
                r.on::<Approve>(move |_, _| Answer::Now(approve));
            }),
        ])
    };
    let (approved, _, decided) = gate(&asks(Some(true)));
    assert_eq!(approved, Gate::Run);
    assert!(matches!(
        &decided[..],
        [
            (_, Verdict::Permission { .. }),
            (_, Verdict::Approval { approved: true })
        ]
    ));
    assert_eq!(names(&decided), ["policy", "approver"]);
    let (refused, _, _) = gate(&asks(Some(false)));
    assert_eq!(
        refused,
        Gate::Refuse("approver did not approve this call".into())
    );
    let (unanswered, _, decided) = gate(&asks(None));
    assert_eq!(unanswered, Gate::Ask);
    assert_eq!(names(&decided), ["policy"]);
    let denies = hooks(vec![one("strict", |r| {
        r.on::<Permission>(|_, _| Level::Deny);
    })]);
    assert_eq!(
        gate(&denies).0,
        Gate::Refuse("strict denied this call".into())
    );
}

/// An `approve` handler that answers later stops the merge at itself: the
/// gate waits with what was decided before, and an answer of none goes on
/// from the handler after it.
#[test]
fn a_later_answer_resumes_the_merge_where_it_stopped() {
    let view = Counted::default();
    let cx = point_cx(&view);
    let told = Arc::new(AtomicUsize::new(0));
    let after = Arc::clone(&told);
    let set = hooks(vec![
        one("policy", |r| {
            r.on::<Permission>(|_, _| Level::Ask);
        }),
        one("none", |r| {
            r.on::<Approve>(|_, _| Answer::Now(None));
        }),
        one("judge", |r| {
            r.on::<Approve>(|_, _| Answer::Later(Later::new(async { None })));
        }),
        one("last", move |r| {
            r.on::<Approve>(move |_, _| {
                after.fetch_add(1, Ordering::SeqCst);
                Answer::Now(Some(false))
            });
        }),
    ]);
    let Gating::Approving(approving) = set.gate(&cx, "bash", &Json::new(&json!({}))) else {
        panic!("the judge answers later")
    };
    assert_eq!(&*approving.by, "judge");
    assert_eq!(approving.next, 2);
    assert_eq!(names(&approving.decisions), ["policy"]);
    // Nothing after the judge was asked.
    assert_eq!(told.load(Ordering::SeqCst), 0);
    assert_eq!(rt::block_on(approving.answer.0), None);
    // Its none goes on from the next handler.
    let input = Json::new(&json!({}));
    let rest = set.approve_from(&cx, "bash", &input, approving.next);
    assert_eq!(rest, Asked::Now("last".into(), false));
    assert_eq!(told.load(Ordering::SeqCst), 1);
    let gated = Gated::approved(
        Json::null(),
        approving.decisions,
        Some(("last".into(), false)),
    );
    assert_eq!(
        gated.gate,
        Gate::Refuse("last did not approve this call".into())
    );
    assert_eq!(names(&gated.decisions), ["policy", "last"]);
    // From past the last handler, no one answers: the user is asked.
    assert_eq!(set.approve_from(&cx, "bash", &input, 3), Asked::None);
    let gated = Gated::approved(Json::null(), Decided::new(), None);
    assert_eq!(gated.gate, Gate::Ask);
}

/// A `step` rewrite is logged, with the model when it changed; an answer
/// ends the chain with who gave it.
#[test]
fn a_step_rewrite_is_logged_and_an_answer_ends_it() {
    let view = Counted::default();
    let cx = point_cx(&view);
    let set = hooks(vec![
        one("fallback", |r| {
            r.on::<Step>(|_, request| {
                Rewrite::Replace(llm::Request {
                    model: "cheap".into(),
                    ..request.clone()
                })
            });
        }),
        one("cache", |r| {
            r.on::<Step>(|_, request| {
                let message = llm::Assistant {
                    content: Vec::new(),
                    origin: None,
                };
                let answer = llm::Response {
                    message,
                    finish: llm::Finish::new(llm::Reason::Stop, None),
                    usage: llm::Usage::default(),
                    id: None,
                    model: Some(request.model.clone()),
                    meta: llm::Meta::new(),
                };
                Rewrite::Stop(answer)
            });
        }),
    ]);
    let mut request = llm::Request {
        model: "big".into(),
        ..llm::Request::default()
    };
    let (decided, answered) = set.step(&cx, &mut request);
    assert_eq!(request.model, "cheap");
    assert!(matches!(
        &decided[..],
        [(by, Verdict::PrepareStep { model: Some(m) })] if &**by == "fallback" && m == "cheap"
    ));
    let (by, answer) = answered.unwrap();
    assert_eq!((&*by, answer.model.as_deref()), ("cache", Some("cheap")));
}

#[test]
fn input_rejections_name_who_and_post_tool_logs_the_original() {
    let view = Counted::default();
    let cx = point_cx(&view);
    let set = hooks(vec![one("a", |r| {
        r.on::<Input>(|_, text| match text {
            "no" => Rewrite::Stop("not that".into()),
            text => Rewrite::Replace(format!("{text}1")),
        })
        .on::<PostTool>(|_, done| {
            assert!(matches!(done.outcome, Outcome::Interrupted { .. }));
            Rewrite::Replace(Outcome::Failed {
                error: done.tool.into(),
            })
        });
    })]);
    let Chained { value, decisions } = set.input(&cx, "hi".to_owned()).unwrap();
    assert_eq!(value, "hi1");
    assert!(matches!(&decisions[..], [(_, Verdict::Input { original })] if original == "hi"));
    assert_eq!(set.input(&cx, "no".to_owned()), Err("a: not that".into()));

    let outcome = Outcome::Interrupted { output: None };
    let Chained { value, decisions } = set.post_tool(&cx, ("t", &Json::null()), outcome);
    assert_eq!(value, Outcome::Failed { error: "t".into() });
    assert!(matches!(
        &decisions[..],
        [(
            _,
            Verdict::PostTool {
                original: Outcome::Interrupted { output: None }
            }
        )]
    ));
}

/// Answers over JSON, as a Lua mod or an extension process would.
struct Erased(fn(&Value) -> Value);

impl ErasedHandler for Erased {
    fn on(&self, _: &PointCx<'_>, args: &Json) -> Json {
        Json::new(&(self.0)(&args.value()))
    }
}

#[test]
fn erased_handlers_answer_in_json_and_a_policy_that_cannot_be_read_denies() {
    let view = Counted::default();
    let cx = point_cx(&view);
    let set = hooks(vec![one("lua", |r| {
        r.on_erased(
            "pre_tool",
            Arc::new(Erased(
                |args| json!({ "replace": { "tool": args["tool"] } }),
            )),
        )
        .on_erased(
            "permission",
            Arc::new(Erased(|args| match args["input"]["tool"].as_str() {
                Some("bash") => json!("ask"),
                _ => json!("maybe"),
            })),
        );
        assert_eq!(r.errors, Vec::<String>::new());
    })]);
    let gated = set.gate(&cx, "bash", &Json::new(&json!({}))).now().unwrap();
    assert_eq!(
        (gated.gate, gated.input.value()),
        (Gate::Ask, json!({ "tool": "bash" }))
    );
    let gated = set.gate(&cx, "edit", &Json::new(&json!({}))).now().unwrap();
    assert_eq!(gated.gate, Gate::Refuse("lua denied this call".into()));

    let mut r = Registrar::default();
    r.on_erased("spawn", Arc::new(Erased(|_| Value::Null)));
    assert_eq!(r.errors, ["point spawn takes no erased handlers"]);
}
