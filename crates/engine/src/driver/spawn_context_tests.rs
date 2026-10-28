use super::*;
use crate::store::{Header, Jsonl, LogStore};
use crate::transcript::Transcript;
use protocol::{Entry, Epoch, Mode, Seq};

#[test]
fn normalization_never_changes_frozen_settings_or_authority() {
    let original = crate::tests::spawn_intent(17, "frozen prompt");
    let before = protocol::publication::SpawnImage::from_json(&original).unwrap();
    let mut intent = SpawnIntent::from_json(&original).unwrap();
    intent.image = crate::context::test_append(&[Entry::new(
        Seq::new(1).unwrap(),
        Epoch(0),
        Body::Assistant {
            text: "context".into(),
        },
    )]);
    let after = protocol::publication::SpawnImage::from_json(&intent.into_json().unwrap()).unwrap();
    assert_eq!(before.authority, after.authority);
    let restored = SpawnIntent::from_json(&after.to_json().unwrap()).unwrap();
    assert_eq!(restored.call, CallId::new(17).unwrap());
    assert_eq!(restored.request.prompt, "frozen prompt");
    assert_eq!(restored.request.depth.remaining(), 7);
}

#[test]
fn context_survives_removal_of_ancestor_files() {
    let root = std::env::temp_dir().join(format!("a-context-image-{}", SessionId::new()));
    std::fs::create_dir(&root).unwrap();
    let ancestors = root.join("ancestors");
    let children = root.join("children");
    let ancestor_store = Jsonl::open(&ancestors).unwrap();
    let child_store = Jsonl::open(&children).unwrap();
    let mut header = Header {
        id: SessionId::new(),
        workspace: root.clone(),
        parent: None,
    };
    let mut parent = ancestor_store.create(&header).unwrap();
    parent
        .append(Body::Input {
            segments: "ancestor".into(),
            mode: Mode::Queue,
            taken: true,
            origin: None,
            force: false,
        })
        .unwrap();
    let mut intent = SpawnIntent::from_json(&crate::tests::spawn_intent(1, "child")).unwrap();
    intent.image = crate::context::test_append(&ancestor_store.entries(header.id).unwrap());
    let nested_id = SessionId::new();
    let mut nested = ancestor_store
        .create(&Header {
            id: nested_id,
            ..header.clone()
        })
        .unwrap();
    nested
        .append(Body::Spawned {
            intent: intent.into_json().unwrap(),
        })
        .unwrap();
    let entries = ancestor_store.entries(nested_id).unwrap();
    let expected = Transcript::of(&entries, Arc::new(Registry::default()), "").into_context();
    let mut intent = SpawnIntent::from_json(&crate::tests::spawn_intent(1, "grandchild")).unwrap();
    intent.image = crate::context::test_append(&entries);
    header.id = SessionId::new();
    let mut child = child_store.create(&header).unwrap();
    child
        .append(Body::Spawned {
            intent: intent.into_json().unwrap(),
        })
        .unwrap();
    drop((parent, nested, child, ancestor_store, child_store));
    std::fs::remove_dir_all(&ancestors).unwrap();
    let reopened = Jsonl::open(&children).unwrap();
    let actual = Transcript::of(
        reopened.entries(header.id).unwrap(),
        Arc::new(Registry::default()),
        "",
    )
    .into_context();
    assert_eq!(actual, expected);
    assert_eq!(
        actual,
        llm::Messages::from_iter([llm::Message::user("ancestor")])
    );
    drop(reopened);
    std::fs::remove_dir_all(root).unwrap();
}
