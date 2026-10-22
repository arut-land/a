use super::*;
use protocol::{AttemptId, CallId, Entry, Epoch, Json, Seq};

fn fact(session: SessionId, attempt: u16, start: u64, elapsed: Option<u64>) -> CallFact {
    CallFact {
        id: AttemptId {
            executing_session: session,
            call: CallId::new(1).unwrap(),
            attempt,
        },
        model: "test".into(),
        role: Json::encode(&"session").unwrap(),
        runner: "agent".into(),
        agent: "loop".into(),
        side: false,
        unix_ms: 100,
        clock_epoch: session,
        start_ns: start,
        elapsed_ns: elapsed,
        usage: Some(
            Json::encode(&llm::Usage {
                input: Some(10),
                output: Some(2),
                ..llm::Usage::default()
            })
            .unwrap(),
        ),
        charge: None,
    }
}
fn log(session: SessionId, parent: Option<SessionId>, facts: Vec<CallFact>) -> ext::LogPrefix {
    ext::LogPrefix {
        session,
        parent,
        entries: facts
            .into_iter()
            .enumerate()
            .map(|(i, fact)| {
                Entry::new(
                    Seq::new(i as u64 + 1).unwrap(),
                    Epoch(0),
                    Body::Fact {
                        fact: ExtensionFact::Call(Box::new(fact)),
                    },
                )
            })
            .collect(),
    }
}
fn prices() -> Prices {
    Prices {
        version: "test-v1".into(),
        currency: "USD".into(),
        models: BTreeMap::from([("test".into(), [1, 2, 0, 0, 0, 0, 0, 0, 0, 0])]),
    }
}
#[test]
fn retries_count_each_executed_attempt_once() {
    let id = SessionId::new();
    let first = fact(id, 0, 0, Some(10));
    let mut start = first.clone();
    start.elapsed_ns = None;
    start.usage = None;
    let mut p = Projection::default();
    p.fold(&log(
        id,
        None,
        vec![start, first.clone(), first, fact(id, 1, 10, Some(10))],
    ));
    let total = p.own(id, &prices());
    assert_eq!(total.attempts, 2);
    assert_eq!(
        total.usage[0],
        UsageTotal {
            known: 20,
            missing: 0
        }
    );
    assert_eq!(total.cost.known, 28);
}
#[test]
fn unknown_usage_is_never_zero() {
    let id = SessionId::new();
    let mut f = fact(id, 0, 0, None);
    f.usage = None;
    let mut p = Projection::default();
    p.fold(&log(id, None, vec![f]));
    let total = p.own(id, &prices());
    assert_eq!(
        total.usage[0],
        UsageTotal {
            known: 0,
            missing: 1
        }
    );
    assert_eq!(total.cost.missing, 1);
    assert_eq!(total.call_ns.missing, 1);
}
#[test]
fn cached_replay_and_copied_forks_add_no_spend() {
    let id = SessionId::new();
    let fork = SessionId::new();
    let f = fact(id, 0, 0, Some(10));
    let original = log(id, None, vec![f.clone()]);
    let mut p = Projection::default();
    p.fold(&original);
    p.fold(&original);
    p.fold(&log(fork, None, vec![f]));
    assert_eq!(p.own(id, &prices()).attempts, 1);
    assert_eq!(p.own(fork, &prices()).attempts, 0);
}
#[test]
fn subtree_totals_count_descendants_once() {
    let root = SessionId::new();
    let child = SessionId::new();
    let grandchild = SessionId::new();
    let mut p = Projection::default();
    for (id, parent) in [(root, None), (child, Some(root)), (grandchild, Some(child))] {
        let mut f = fact(id, 0, 0, Some(10));
        f.side = id == grandchild;
        let prefix = log(id, parent, vec![f]);
        p.fold(&prefix);
        p.fold(&prefix);
    }
    assert_eq!(p.subtree(root, &prices()).attempts, 3);
    assert_eq!(p.own(root, &prices()).attempts, 1);
    assert_eq!(
        p.query(
            root,
            true,
            &Filter {
                side: Some(true),
                ..Filter::default()
            },
            &prices()
        )
        .attempts,
        1
    );
    assert_eq!(
        p.query(
            root,
            true,
            &Filter {
                agent: Some("other".into()),
                ..Filter::default()
            },
            &prices()
        )
        .attempts,
        0
    );
}
#[test]
fn overlap_changes_critical_path_but_not_summed_call_time() {
    let id = SessionId::new();
    let mut p = Projection::default();
    p.fold(&log(
        id,
        None,
        vec![fact(id, 0, 0, Some(10)), fact(id, 1, 10, Some(10))],
    ));
    let sequential = p.own(id, &prices());
    let mut p = Projection::default();
    p.fold(&log(
        id,
        None,
        vec![fact(id, 0, 0, Some(10)), fact(id, 1, 0, Some(10))],
    ));
    let overlap = p.own(id, &prices());
    assert_eq!(sequential.call_ns, overlap.call_ns);
    assert_eq!(sequential.critical_ns.known, 20);
    assert_eq!(overlap.critical_ns.known, 10);
}
#[test]
fn repricing_changes_no_execution_fact() {
    let id = SessionId::new();
    let prefix = log(id, None, vec![fact(id, 0, 0, Some(10))]);
    let encoded = Json::encode(&prefix.entries).unwrap();
    let mut p = Projection::default();
    p.fold(&prefix);
    let first = p.own(id, &prices());
    let mut new = prices();
    new.version = "test-v2".into();
    new.models.get_mut("test").unwrap()[0] = 3;
    let second = p.own(id, &new);
    assert_eq!(second.cost.known - first.cost.known, 20);
    assert_eq!(second.usage, first.usage);
    assert_eq!(Json::encode(&prefix.entries).unwrap(), encoded);
    let mut charged = fact(id, 1, 10, Some(10));
    charged.charge = Some(protocol::Charge {
        currency: "USD".into(),
        nanos: 7,
    });
    p.fold(&log(id, None, vec![fact(id, 0, 0, Some(10)), charged]));
    assert_eq!(p.own(id, &new).cost.known, second.cost.known + 7);
}
#[test]
fn metrics_rebuild_equals_incremental_fold() {
    let id = SessionId::new();
    let first = log(id, None, vec![fact(id, 0, 0, None)]);
    let full = log(
        id,
        None,
        vec![
            fact(id, 0, 0, None),
            fact(id, 0, 0, Some(10)),
            fact(id, 1, 10, Some(20)),
        ],
    );
    let mut incremental = Projection::default();
    incremental.fold(&first);
    assert_eq!(incremental.own(id, &prices()).call_ns.missing, 1);
    incremental.fold(&full);
    let mut rebuilt = Projection::default();
    rebuilt.fold(&full);
    assert_eq!(incremental.own(id, &prices()), rebuilt.own(id, &prices()));
}
#[test]
fn restart_gaps_remain_unknown() {
    let id = SessionId::new();
    let mut p = Projection::default();
    let mut restarted = fact(id, 1, 0, Some(10));
    restarted.clock_epoch = SessionId::new();
    p.fold(&log(
        id,
        None,
        vec![fact(id, 0, 0, Some(10)), restarted, fact(id, 2, 0, None)],
    ));
    let total = p.own(id, &prices());
    assert_eq!(
        total.call_ns,
        UsageTotal {
            known: 20,
            missing: 1
        }
    );
    assert_eq!(
        total.critical_ns,
        UsageTotal {
            known: 20,
            missing: 2
        }
    );
}

#[test]
fn pricing_adjustments_require_observed_subsets() {
    let id = SessionId::new();
    let mut f = fact(id, 0, 0, Some(10));
    let mut table = prices();
    table.models.get_mut("test").unwrap()[2] = -1;
    assert_eq!(
        table.cost(&f, f.usage.as_ref().unwrap().parse().unwrap()),
        None
    );
    f.usage = Some(
        Json::encode(&llm::Usage {
            input: Some(10),
            output: Some(2),
            cache_read: Some(4),
            ..llm::Usage::default()
        })
        .unwrap(),
    );
    assert_eq!(
        table.cost(&f, f.usage.as_ref().unwrap().parse().unwrap()),
        Some(10)
    );
    let mut p = Projection::default();
    p.fold(&log(id, None, vec![f]));
    assert_eq!(
        p.query(
            id,
            false,
            &Filter {
                call: CallId::new(2),
                ..Filter::default()
            },
            &table
        )
        .attempts,
        0
    );
}

#[test]
fn replaced_sequence_coverage_invalidates_cached_facts() {
    let id = SessionId::new();
    let mut p = Projection::default();
    p.fold(&log(id, None, vec![fact(id, 0, 0, Some(10))]));
    assert_eq!(p.own(id, &prices()).call_ns.known, 10);
    let mut replacement = log(id, None, vec![fact(id, 0, 0, Some(20))]);
    replacement.entries[0].epoch = Epoch(1);
    p.fold(&replacement);
    assert_eq!(p.own(id, &prices()).call_ns.known, 20);
}
