use super::*;
use protocol::{AttemptId, Body, CallId, Entry, Epoch, Json, Seq};

fn fact(session: SessionId, attempt: u16, start: u64, elapsed: Option<u64>) -> CallFact {
    CallFact {
        id: AttemptId {
            executing_session: session,
            call: CallId::new(1).unwrap(),
            attempt,
        },
        model: "test".into(),
        role: "session".into(),
        runner: "agent".into(),
        agent: "loop".into(),
        side: false,
        unix_ms: 100,
        clock_epoch: session,
        start_ns: start,
        elapsed_ns: elapsed,
        usage: Some(
            llm::Usage::encode_counts(
                &llm::Usage {
                    input: Some(10),
                    output: Some(2),
                    ..llm::Usage::default()
                }
                .counts(),
            )
            .unwrap(),
        ),
        charge: None,
    }
}
fn log(session: SessionId, parent: Option<SessionId>, facts: Vec<CallFact>) -> ext::LogPrefix {
    ext::LogPrefix {
        session,
        parent,
        cursor: ext::LogCursor {
            seq: facts.len() as u64,
            ..ext::LogCursor::default()
        },
        reset: true,
        inherited_through: 0,
        entries: facts
            .into_iter()
            .enumerate()
            .map(|(i, fact)| {
                let mut entry = Entry::new(
                    Seq::new(i as u64 + 1).unwrap(),
                    Epoch(0),
                    Body::ModelResult {
                        call: fact.id.call,
                        r#for: None,
                        outcome: protocol::Outcome::Interrupted { output: None },
                        failure: None,
                    },
                );
                entry.execution = Some(fact.to_json().unwrap());
                entry
            })
            .collect(),
    }
}
fn prices() -> Prices {
    Prices {
        version: "test-v1".into(),
        currency: "USD".into(),
        models: Vec::from([("test".into(), [1, 2, 0, 0, 0, 0, 0, 0, 0, 0])]),
    }
}
#[test]
fn retries_count_each_executed_attempt_once() {
    let id = SessionId::new();
    let first = fact(id, 0, 0, Some(10));
    let mut p = Projection::default();
    p.fold(&log(
        id,
        None,
        vec![first.clone(), first, fact(id, 1, 10, Some(10))],
    ));
    let total = p.own(id, &prices());
    assert_eq!(total.attempts.executed, 2);
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
    assert_eq!(p.own(id, &prices()).attempts.executed, 1);
    assert_eq!(p.own(fork, &prices()).attempts.executed, 0);
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
    assert_eq!(p.subtree(root, &prices()).attempts.executed, 3);
    assert_eq!(p.own(root, &prices()).attempts.executed, 1);
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
        .attempts
        .executed,
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
        .attempts
        .executed,
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
    new.models[0].1[0] = 3;
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
    let first = log(id, None, vec![fact(id, 0, 0, Some(10))]);
    let full = log(
        id,
        None,
        vec![fact(id, 0, 0, Some(10)), fact(id, 1, 10, Some(20))],
    );
    let mut incremental = Projection::default();
    incremental.fold(&first);
    assert_eq!(incremental.own(id, &prices()).call_ns.known, 10);
    let mut suffix = log(id, None, vec![fact(id, 1, 10, Some(20))]);
    suffix.entries[0].seq = Seq::new(2).unwrap();
    suffix.cursor.seq = 2;
    suffix.reset = false;
    incremental.fold(&suffix);
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
    table.models[0].1[2] = -1;
    assert_eq!(
        table.cost(
            &f,
            llm::Usage::decode_counts(f.usage.as_ref().unwrap()).unwrap()
        ),
        None
    );
    f.usage = Some(
        llm::Usage::encode_counts(
            &llm::Usage {
                input: Some(10),
                output: Some(2),
                cache_read: Some(4),
                ..llm::Usage::default()
            }
            .counts(),
        )
        .unwrap(),
    );
    assert_eq!(
        table.cost(
            &f,
            llm::Usage::decode_counts(f.usage.as_ref().unwrap()).unwrap()
        ),
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
        .attempts
        .executed,
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

#[test]
fn an_unsettled_intent_has_unknown_spend_without_claiming_execution() {
    let id = SessionId::new();
    let mut prefix = log(id, None, vec![]);
    prefix.entries.push(Entry::new(
        Seq::new(1).unwrap(),
        Epoch(0),
        Body::ModelCall {
            model: "test".into(),
            attempt: 0,
            choice: protocol::ToolChoice::Auto,
            r#for: None,
            purpose: None,
            request: None,
        },
    ));
    let mut projection = Projection::default();
    projection.fold(&prefix);
    let total = projection.own(id, &prices());
    assert_eq!(
        total.attempts,
        Attempts {
            executed: 0,
            ambiguous: 1
        }
    );
    assert_eq!(total.usage[0].missing, 1);
    assert_eq!(total.call_ns.missing, 1);
}

#[test]
fn unrelated_suffixes_preserve_the_accounting_cache() {
    let id = SessionId::new();
    let mut projection = Projection::default();
    projection.fold(&log(id, None, vec![fact(id, 0, 0, Some(10))]));
    let before = projection.own(id, &prices());
    let suffix = ext::LogPrefix {
        session: id,
        parent: None,
        reset: false,
        inherited_through: 0,
        cursor: ext::LogCursor {
            seq: 2,
            ..ext::LogCursor::default()
        },
        entries: vec![Entry::new(
            Seq::new(2).unwrap(),
            Epoch(0),
            Body::Assistant {
                text: "unrelated".into(),
            },
        )],
    };
    projection.fold(&suffix);
    assert!(projection.cached.is_some());
    assert_eq!(projection.cursors()[0].1.seq, 2);
    assert_eq!(projection.own(id, &prices()), before);
}

#[test]
fn copied_interrupted_intents_do_not_create_unknown_spend() {
    let id = SessionId::new();
    let mut prefix = log(id, None, vec![fact(id, 0, 0, Some(10))]);
    prefix.entries.insert(
        0,
        Entry::new(
            Seq::new(1).unwrap(),
            Epoch(0),
            Body::ModelCall {
                model: "test".into(),
                attempt: 0,
                choice: protocol::ToolChoice::Auto,
                r#for: None,
                purpose: None,
                request: None,
            },
        ),
    );
    prefix.entries[1].seq = Seq::new(3).unwrap();
    prefix.inherited_through = 2;
    prefix.cursor.seq = 3;
    let mut projection = Projection::default();
    projection.fold(&prefix);
    let total = projection.own(id, &prices());
    assert_eq!(total.attempts.executed, 1);
    assert_eq!(total.usage[0].missing, 0);
}

#[test]
fn overlapping_suffixes_do_not_duplicate_uncertain_intents() {
    let id = SessionId::new();
    let mut prefix = log(id, None, vec![]);
    prefix.entries.push(Entry::new(
        Seq::new(1).unwrap(),
        Epoch(0),
        Body::ModelCall {
            model: "test".into(),
            attempt: 0,
            choice: protocol::ToolChoice::Auto,
            r#for: None,
            purpose: None,
            request: None,
        },
    ));
    prefix.cursor.seq = 1;
    prefix.reset = false;
    let mut projection = Projection::default();
    projection.fold(&prefix);
    projection.fold(&prefix);
    assert_eq!(projection.own(id, &prices()).usage[0].missing, 1);
}

#[test]
fn successful_recovery_keeps_the_crashed_attempts_usage_unknown() {
    let id = SessionId::new();
    let mut prefix = log(id, None, vec![fact(id, 1, 10, Some(10))]);
    prefix.entries[0].seq = Seq::new(3).unwrap();
    prefix.entries.splice(
        0..0,
        [
            Entry::new(
                Seq::new(1).unwrap(),
                Epoch(0),
                Body::ModelCall {
                    model: "test".into(),
                    attempt: 0,
                    choice: protocol::ToolChoice::Auto,
                    r#for: None,
                    purpose: None,
                    request: None,
                },
            ),
            Entry::new(
                Seq::new(2).unwrap(),
                Epoch(1),
                Body::Decision {
                    call: CallId::new(1),
                    by: "retry".into(),
                    d: protocol::Verdict::Retry {
                        attempt: 1,
                        error: "daemon stopped during attempt zero".into(),
                    },
                },
            ),
        ],
    );
    prefix.entries[2].epoch = Epoch(1);
    prefix.entries[2].body = Body::ModelResult {
        call: CallId::new(1).unwrap(),
        r#for: None,
        outcome: protocol::Outcome::Done {
            output: Json::encode(&"recovered").unwrap(),
        },
        failure: None,
    };
    prefix.cursor.seq = 3;
    let mut projection = Projection::default();
    projection.fold(&prefix);
    let total = projection.own(id, &prices());
    assert_eq!(
        total.attempts,
        Attempts {
            executed: 1,
            ambiguous: 1
        }
    );
    assert_eq!(
        total.usage[0],
        UsageTotal {
            known: 10,
            missing: 1
        }
    );
}

#[test]
fn supplied_prices_are_sorted_and_duplicates_are_rejected() {
    let prices =
        Prices::parse(r#"["v1","USD",[["z",[0,0,0,0,0,0,0,0,0,0]],["a",[0,0,0,0,0,0,0,0,0,0]]]]"#)
            .unwrap();
    assert_eq!(prices.models[0].0, "a");
    assert!(
        Prices::parse(r#"["v1","USD",[["a",[0,0,0,0,0,0,0,0,0,0]],["a",[0,0,0,0,0,0,0,0,0,0]]]]"#)
            .is_err()
    );
}

#[test]
fn unreadable_settlement_metadata_preserves_uncertainty() {
    let id = SessionId::new();
    let mut prefix = log(id, None, vec![]);
    prefix.entries = vec![
        Entry::new(
            Seq::new(1).unwrap(),
            Epoch(0),
            Body::ModelCall {
                model: "test".into(),
                attempt: 0,
                choice: protocol::ToolChoice::Auto,
                r#for: None,
                purpose: None,
                request: None,
            },
        ),
        Entry::new(
            Seq::new(2).unwrap(),
            Epoch(0),
            Body::ModelResult {
                call: CallId::new(1).unwrap(),
                r#for: None,
                outcome: protocol::Outcome::Interrupted { output: None },
                failure: None,
            },
        ),
    ];
    prefix.entries[1].execution = Some(Json::encode(&["unreadable"]).unwrap());
    let mut projection = Projection::default();
    projection.fold(&prefix);
    let total = projection.own(id, &prices());
    assert_eq!(total.attempts.executed, 0);
    assert_eq!(total.usage[0].missing, 1);
}

#[test]
fn a_retry_without_execution_settlement_remains_unknown_after_restart() {
    let id = SessionId::new();
    let mut prefix = log(id, None, vec![fact(id, 0, 0, Some(10))]);
    prefix.entries[0].seq = Seq::new(2).unwrap();
    prefix.entries[0].body = Body::Decision {
        call: CallId::new(1),
        by: "retry".into(),
        d: protocol::Verdict::Retry {
            attempt: 1,
            error: "retryable".into(),
        },
    };
    prefix.entries.insert(
        0,
        Entry::new(
            Seq::new(1).unwrap(),
            Epoch(0),
            Body::ModelCall {
                model: "test".into(),
                attempt: 0,
                choice: protocol::ToolChoice::Auto,
                r#for: None,
                purpose: None,
                request: None,
            },
        ),
    );
    prefix.entries.push(Entry::new(
        Seq::new(3).unwrap(),
        Epoch(1),
        Body::ModelResult {
            call: CallId::new(1).unwrap(),
            r#for: None,
            outcome: protocol::Outcome::Interrupted { output: None },
            failure: None,
        },
    ));
    let mut projection = Projection::default();
    projection.fold(&prefix);
    let total = projection.own(id, &prices());
    assert_eq!(total.attempts.executed, 1);
    assert_eq!(
        total.usage[0],
        UsageTotal {
            known: 10,
            missing: 1
        }
    );
    assert_eq!(
        total.call_ns,
        UsageTotal {
            known: 10,
            missing: 1
        }
    );
}
