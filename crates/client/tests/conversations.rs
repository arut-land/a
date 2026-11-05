use client::{ConversationHead, ConversationStatus, ConversationTree};
use protocol::{
    Activity, SessionId,
    history::{Child, Conversation},
    ledger::OutcomeKind,
    view::Usage,
};

#[test]
fn conversation_family_survives_history_release_and_pins_the_viewed_child() {
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

#[test]
fn terminal_rows_use_their_state_grace_interval_and_pin_the_viewed_child() {
    use std::time::{Duration, Instant};
    let main = SessionId::new();
    let child = SessionId::new();
    let now = Instant::now();
    for (outcome, seconds) in [
        (OutcomeKind::Done, 5),
        (OutcomeKind::Failed, 10),
        (OutcomeKind::Interrupted, 30),
    ] {
        let retention = Duration::from_secs(seconds);
        let head = ConversationHead {
            session: child,
            seq: 1,
            identity: Conversation {
                parent: Some(main),
                outcome: Some(outcome),
                ..Default::default()
            },
            usage: Usage::default(),
        };
        let root = ConversationHead {
            session: main,
            seq: 1,
            identity: Conversation::default(),
            usage: Usage::default(),
        };
        let mut tree = ConversationTree::new(main);
        tree.apply_at(&root, Some(Activity::Idle), now, 10000);
        tree.apply_at(&head, Some(Activity::Idle), now, 10000);
        assert_eq!(tree.next_expiry(now), Some(now + retention));
        assert_eq!(
            tree.rows_at(now + retention.saturating_sub(Duration::from_millis(1)))
                .len(),
            2
        );
        tree.apply_at(&head, None, now + Duration::from_secs(4), 10000);
        assert_eq!(tree.rows_at(now + retention).len(), 1);
        assert_eq!(tree.next_expiry(now + retention), None);
        let resumed = now + retention + Duration::from_secs(1);
        tree.apply_at(&head, Some(Activity::Running), resumed, 10000);
        assert_eq!(tree.rows_at(resumed).len(), 2);
        let terminal = resumed + Duration::from_secs(1);
        tree.apply_at(&head, Some(Activity::Idle), terminal, 10000);
        assert_eq!(tree.next_expiry(terminal), Some(terminal + retention));
        let mut viewed = ConversationTree::new(child);
        viewed.apply_at(&root, Some(Activity::Idle), now, 10000);
        viewed.apply_at(&head, Some(Activity::Idle), now, 10000);
        assert_eq!(viewed.rows_at(now + Duration::from_secs(60)).len(), 2);
        assert_eq!(viewed.next_expiry(now), None);
        viewed.view(main);
        assert_eq!(viewed.rows_at(now + Duration::from_secs(60)).len(), 1);
        viewed.view(child);
        assert_eq!(viewed.rows_at(now + Duration::from_secs(60)).len(), 2);
    }
}

#[test]
fn reopening_an_expired_child_only_pins_the_view_that_is_open() {
    use std::time::{Duration, Instant};
    let main = SessionId::new();
    let child = SessionId::new();
    let now = Instant::now();
    let root = ConversationHead {
        session: main,
        seq: 1,
        identity: Conversation::default(),
        usage: Usage::default(),
    };
    let head = ConversationHead {
        session: child,
        seq: 1,
        identity: Conversation {
            parent: Some(main),
            outcome: Some(OutcomeKind::Done),
            settled_unix_ms: Some(1),
            ..Default::default()
        },
        usage: Usage::default(),
    };
    for (origin, expected) in [(main, 1), (child, 2)] {
        let mut tree = ConversationTree::new(origin);
        tree.apply_at(&root, Some(Activity::Idle), now, 10000);
        tree.apply_at(&head, Some(Activity::Idle), now, 10000);
        assert_eq!(tree.rows_at(now).len(), expected);
        assert_eq!(tree.next_expiry(now + Duration::from_secs(1)), None);
    }
}
