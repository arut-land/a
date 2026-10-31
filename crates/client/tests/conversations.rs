use client::{ConversationHead, ConversationStatus, ConversationTree};
use protocol::{
    Activity, SessionId,
    history::{Child, Conversation},
    ledger::OutcomeKind,
    view::Usage,
};

#[test]
fn conversation_family_survives_history_release_and_keeps_finished_children() {
    let main = SessionId::new();
    let child = SessionId::new();
    let grandchild = SessionId::new();
    let mut tree = ConversationTree::new(child);
    let main_head = ConversationHead {
        session: main,
        seq: 1,
        identity: Conversation {
            children: vec![Child {
                session: child,
                agent: "worker".into(),
                title: "Inspect the app".into(),
                admitted_unix_ms: 1000,
            }],
            ..Default::default()
        },
        usage: Usage::default(),
    };
    let mut child_head = ConversationHead {
        session: child,
        seq: 2,
        identity: Conversation {
            parent: Some(main),
            agent: Some("worker".into()),
            title: Some("Inspect the app".into()),
            admitted_unix_ms: Some(1000),
            children: vec![Child {
                session: grandchild,
                agent: "worker".into(),
                title: "Measure".into(),
                admitted_unix_ms: 2000,
            }],
            ..Default::default()
        },
        usage: Usage {
            input: 40000,
            output: 400,
        },
    };
    tree.apply(&main_head, Some(Activity::Idle));
    tree.apply(&child_head, Some(Activity::Running));
    let rows = tree.rows();
    assert_eq!(
        rows.iter()
            .map(|row| (row.session, row.depth))
            .collect::<Vec<_>>(),
        [(main, 0), (child, 1), (grandchild, 2)]
    );
    assert_eq!(rows[1].status, ConversationStatus::Running);
    assert!(rows[1].detail(16000).contains("15s · 40400 tokens"));
    child_head.identity.outcome = Some(OutcomeKind::Interrupted);
    child_head.identity.elapsed_ns = Some(8_000_000_000);
    tree.apply(&child_head, Some(Activity::Idle));
    tree.apply(&main_head, Some(Activity::Idle));
    tree.apply(&child_head, None);
    let rows = tree.rows();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].status, ConversationStatus::Interrupted);
    assert_eq!(rows[1].detail(999_999), "interrupted · 8s · 40400 tokens");
    child_head.identity.outcome = Some(OutcomeKind::Failed);
    tree.apply(&child_head, Some(Activity::Idle));
    assert_eq!(tree.rows()[1].status, ConversationStatus::Failed);
}
