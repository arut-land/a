use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use protocol::Permission as Level;
use serde_json::json;

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
            PrepareStep,
            RequestError,
            PreTool,
            Permission,
            Approve,
            Spawn,
            PostTool,
            OnStop,
            Observe,
            SessionStart,
            SessionEnd
        )
    };
}

/// The design's merge table ("Hooks intercept only at decision points").
const TABLE: &[(&str, Merge)] = &[
    ("input", Merge::Chain),
    ("prepare_step", Merge::Chain),
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
];

/// A chain point merges as a chain: each handler sees what the last left,
/// every replacement is reported with what it replaced, and the first stop
/// ends it.
fn chains<P, T, S>(facts: P::Facts<'_>, [start, a, b]: [T; 3], stop: Option<S>)
where
    P: Point<Out = Rewrite<T, S>>,
    T: Clone + PartialEq + Debug + Send + Sync + 'static,
    S: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let view = Counted::default();
    let cx = point_cx(&view);
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
    let merged = set.run::<P>(&cx, facts, &mut subject, &mut |name, before, after| {
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
    let merged = set.run::<P>(&cx, facts, &mut subject, &mut |_, _, _| {});
    let Err((who, why)) = merged else {
        panic!("{} did not stop", P::NAME)
    };
    assert_eq!((&*who, why, subject), ("stop", stop, a), "{}", P::NAME);
    assert_eq!(asked.load(Ordering::SeqCst), 0, "{}", P::NAME);
}

/// A first-answer point takes the first answer, and asks no one after.
fn first<P, T>(facts: P::Facts<'_>, [a, b]: [T; 2])
where
    P: Point<Out = Option<T>>,
    T: Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let view = Counted::default();
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
    let merged = set.run::<P>(&point_cx(&view), facts, &mut (), &mut |_, (), ()| {});
    assert_eq!(
        merged.map(|(name, _)| name.to_string()),
        Some("a".into()),
        "{}",
        P::NAME
    );
    assert_eq!(asked.load(Ordering::SeqCst), 0, "{}", P::NAME);
}

/// An observed point tells every handler, in order.
fn observes<P: Point<Out = ()>>(facts: P::Facts<'_>) {
    let view = Counted::default();
    let told = Arc::new(Mutex::new(Vec::new()));
    let by = ["a", "b", "c"].map(|name| {
        let told = Arc::clone(&told);
        one(name, move |r| {
            r.on::<P>(move |_, _| told.lock().unwrap().push(name));
        })
    });
    hooks(by.into()).run::<P>(&point_cx(&view), facts, &mut (), &mut |_, (), ()| {});
    assert_eq!(*told.lock().unwrap(), ["a", "b", "c"], "{}", P::NAME);
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
    let input = json!({});
    let call = ToolUse {
        tool: "t",
        input: &input,
    };
    let texts = ["0", "a", "b"].map(str::to_owned);
    let stop = Some("no".to_owned());
    chains::<Input, _, _>((), texts, stop.clone());
    chains::<PrepareStep, _, _>((), ["0", "a", "b"].map(request), None);
    chains::<PreTool, _, _>("t", [json!(0), json!(1), json!(2)], stop);
    chains::<Spawn, _, _>((), ["0", "a", "b"].map(spawn), None);
    chains::<PostTool, _, _>(call, ["0", "a", "b"].map(failed), None);

    let error = Failed {
        request: &llm::Request::default(),
        error: "overflow",
    };
    first::<RequestError, _>(error, [request("a"), request("b")]);
    first::<Approve, _>(call, [true, false]);
    first::<OnStop, _>(&Vec::new(), ["a", "b"].map(str::to_owned));

    let entry = Entry::new(1, 0, protocol::Body::Interrupt);
    observes::<Observe>(&entry);
    observes::<SessionStart>(());
    observes::<SessionEnd>(());

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
    let input = json!({ "n": 0 });
    // Handlers elsewhere are not asked, nor is the context read.
    let elsewhere = hooks(vec![one("other", |r| {
        r.on::<OnStop>(|cx, _| Some(cx.model()));
    })]);
    for hooks in [Hooks::default(), elsewhere] {
        let mut decided = Decided::new();
        let mut text = "hi".to_owned();
        let mut subject = input.clone();
        let mut outcome = Outcome::Interrupted { output: None };
        let mut request = llm::Request::default();
        assert_eq!(hooks.input(&cx, &mut text, &mut decided), Ok(()));
        assert_eq!(
            hooks.gate(&cx, "t", &mut subject, &mut decided),
            Ok(Gate::Run)
        );
        hooks.prepare_step(&cx, 1, &mut request, &mut decided);
        let retry = hooks.request_error(&cx, (1, 1), &request, "e", &mut decided);
        assert!(retry.is_none());
        hooks.post_tool(&cx, 1, ("t", &input), &mut outcome, &mut decided);
        hooks.observe(&cx, &Entry::new(1, 0, protocol::Body::Interrupt));
        hooks.session_start(&cx);
        hooks.session_end(&cx);
        assert_eq!((text.as_str(), &subject), ("hi", &input));
        assert_eq!(decided, Decided::new());
    }
    assert_eq!(view.0.load(Ordering::SeqCst), 0);
    // A handler that reads its context reads it then.
    let reads = hooks(vec![one("reader", |r| {
        r.on::<OnStop>(|cx, _| Some(cx.model()));
    })]);
    let said = reads.on_stop(&cx, &Vec::new(), &mut Decided::new());
    assert_eq!(said.as_deref(), Some("test:model"));
    assert_eq!(view.0.load(Ordering::SeqCst), 1);
}

#[test]
fn gate_logs_what_the_chain_and_the_policy_decided() {
    let view = Counted::default();
    let cx = point_cx(&view);
    let bump = |r: &mut Registrar| {
        r.on::<PreTool>(|_, call| {
            Rewrite::Replace(json!({ "n": call.input["n"].as_i64().unwrap() + 1 }))
        });
    };
    let gate = |hooks: &Hooks| {
        let mut input = json!({ "n": 0 });
        let mut decided = Decided::new();
        let verdict = hooks.gate(&cx, "t", &mut input, &mut decided);
        (verdict, input, decided)
    };
    let (verdict, input, decided) = gate(&hooks(vec![one("a", bump), one("b", bump)]));
    assert_eq!((verdict, input), (Ok(Gate::Run), json!({ "n": 2 })));
    assert_eq!(names(&decided), ["a", "b"]);
    assert!(
        matches!(&decided[1].1, Decision::PreTool { input, .. } if input.value() == json!({ "n": 2 }))
    );

    let blocked = hooks(vec![
        one("a", bump),
        one("b", |r| {
            r.on::<PreTool>(|_, _| Rewrite::Stop("no".into()));
        }),
    ]);
    let (verdict, _, decided) = gate(&blocked);
    assert_eq!(verdict, Err("no".into()));
    assert!(
        matches!(&decided[..], [(_, Decision::PreTool { .. }), (b, Decision::Block { .. })] if &**b == "b")
    );

    let asks = |approve: Option<bool>| {
        hooks(vec![
            one("policy", |r| {
                r.on::<Permission>(|_, _| Level::Ask);
            }),
            one("approver", move |r| {
                r.on::<Approve>(move |_, _| approve);
            }),
        ])
    };
    let (approved, _, decided) = gate(&asks(Some(true)));
    assert_eq!(approved, Ok(Gate::Run));
    assert!(matches!(
        &decided[..],
        [
            (_, Decision::Permission { .. }),
            (_, Decision::Approval { approved: true, .. })
        ]
    ));
    assert_eq!(names(&decided), ["policy", "approver"]);
    let (refused, _, _) = gate(&asks(Some(false)));
    assert_eq!(refused, Err("approver did not approve this call".into()));
    let (unanswered, _, decided) = gate(&asks(None));
    assert_eq!(unanswered, Ok(Gate::Ask));
    assert_eq!(names(&decided), ["policy"]);
    let denies = hooks(vec![one("strict", |r| {
        r.on::<Permission>(|_, _| Level::Deny);
    })]);
    assert_eq!(gate(&denies).0, Err("strict denied this call".into()));
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
    let mut decided = Decided::new();
    let mut text = "hi".to_owned();
    set.input(&cx, &mut text, &mut decided).unwrap();
    assert_eq!(text, "hi1");
    assert!(matches!(&decided[..], [(_, Decision::Input { original })] if original == "hi"));
    let mut refused = "no".to_owned();
    assert_eq!(
        set.input(&cx, &mut refused, &mut decided),
        Err("a: not that".into())
    );

    decided.clear();
    let mut outcome = Outcome::Interrupted { output: None };
    set.post_tool(&cx, 7, ("t", &json!({})), &mut outcome, &mut decided);
    assert_eq!(outcome, Outcome::Failed { error: "t".into() });
    assert!(matches!(
        &decided[..],
        [(
            _,
            Decision::PostTool {
                call: 7,
                original: Outcome::Interrupted { output: None }
            }
        )]
    ));
}

/// Answers over JSON, as a Lua mod or an extension process would.
struct Json(fn(&Value) -> Value);

impl ErasedHandler for Json {
    fn on(&self, _: &PointCx<'_>, args: &Value) -> Value {
        (self.0)(args)
    }
}

#[test]
fn erased_handlers_answer_in_json_and_a_policy_that_cannot_be_read_denies() {
    let view = Counted::default();
    let cx = point_cx(&view);
    let set = hooks(vec![one("lua", |r| {
        r.on_erased(
            "pre_tool",
            Arc::new(Json(|args| json!({ "replace": { "tool": args["tool"] } }))),
        )
        .on_erased(
            "permission",
            Arc::new(Json(|args| match args["input"]["tool"].as_str() {
                Some("bash") => json!("ask"),
                _ => json!("maybe"),
            })),
        );
        assert_eq!(r.errors, Vec::<String>::new());
    })]);
    let mut input = json!({});
    let mut decided = Decided::new();
    let gate = set.gate(&cx, "bash", &mut input, &mut decided);
    assert_eq!((gate, input), (Ok(Gate::Ask), json!({ "tool": "bash" })));
    let mut input = json!({});
    let gate = set.gate(&cx, "edit", &mut input, &mut decided);
    assert_eq!(gate, Err("lua denied this call".into()));

    let mut r = Registrar::default();
    r.on_erased("spawn", Arc::new(Json(|_| Value::Null)));
    assert_eq!(r.errors, ["point spawn takes no erased handlers"]);
}
