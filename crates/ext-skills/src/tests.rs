use std::fs;

use ext::host::{Registry, Sources};
use ext::testing::Rig;
use protocol::Body;
use serde_json::json;

use super::*;

fn skills() -> Skills {
    let (name, skill) = parse::parse(
        "---\nname: review\ndescription: Review a change\n---\nPRIVATE BODY",
        std::path::Path::new("/workspace/.a/skills/review"),
    )
    .unwrap();
    Skills {
        catalog: Arc::new(BTreeMap::from([(name, skill)])),
        ..Skills::default()
    }
}

#[test]
fn only_names_and_descriptions_enter_the_index() {
    let registry = Registry::default().with(&skills()).unwrap();
    let index = registry.generation_context().unwrap();
    assert!(index.contains("review: Review a change"));
    assert!(!index.contains("PRIVATE BODY"));
    assert!(!index.contains("/workspace"));
    assert!(!registry.system_prompt().contains("PRIVATE BODY"));
    assert_eq!(
        registry
            .commands()
            .map(ext::DynTool::name)
            .collect::<Vec<_>>(),
        ["review"]
    );
    assert_eq!(registry.tools().specs().len(), 1);
}

#[test]
fn instructions_are_published_once_and_replay_uses_only_the_log() {
    let rig = Rig::new("skills-once", Registry::default().with(&skills()).unwrap());
    assert_eq!(
        rig.done("skill", &json!({"name":"review"})),
        json!({"Ok":"loaded"})
    );
    assert_eq!(
        rig.done("review", &json!({})),
        json!({"Ok":"already_loaded"})
    );
    let log = rig.log.lock().unwrap().clone();
    assert_eq!(
        log.iter()
            .filter(
                |e| matches!(&e.body, Body::Context { text, .. } if text.contains("PRIVATE BODY"))
            )
            .count(),
        1
    );
    assert!(matches!(
        &log[0].body,
        Body::Context {
            publication: Some(_),
            ..
        }
    ));
    rig.restart();
    assert_eq!(
        rig.done("skill", &json!({"name":"review"})),
        json!({"Ok":"already_loaded"})
    );
    assert_eq!(*rig.log.lock().unwrap(), log);
}

#[test]
fn an_unknown_skill_is_a_typed_error_and_publishes_nothing() {
    let rig = Rig::new(
        "skills-unknown",
        Registry::default().with(&Skills::default()).unwrap(),
    );
    assert_eq!(
        rig.done("skill", &json!({"name":"missing"})),
        json!({"Err":{"unknown_skill":{"name":"missing"}}})
    );
    assert!(rig.log.lock().unwrap().is_empty());
}

#[test]
fn a_durable_context_prefix_is_not_published_twice_after_restart() {
    let rig = Rig::new(
        "skills-review-prefix",
        Registry::default().with(&skills()).unwrap(),
    );
    rig.done("skill", &json!({"name":"review"}));
    // A process crash can leave the complete context line without the next marker line.
    rig.log.lock().unwrap().truncate(1);
    rig.restart();
    rig.done("skill", &json!({"name":"review"}));
    assert_eq!(
        rig.log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(&e.body,
            Body::Context { text, .. } if text.contains("PRIVATE BODY")))
            .count(),
        1
    );
}

#[test]
fn instructions_can_be_reloaded_after_compaction_hides_their_context() {
    let rig = Rig::new(
        "skills-review-compact",
        Registry::default().with(&skills()).unwrap(),
    );
    rig.done("skill", &json!({"name":"review"}));
    rig.cx().edit(
        protocol::Seq::new(1).unwrap(),
        protocol::Seq::new(2),
        Some("summary"),
    );
    rig.restart();
    assert_eq!(
        rig.done("skill", &json!({"name":"review"})),
        json!({"Ok":"loaded"})
    );
}

#[test]
fn portable_yaml_is_parsed_and_ambiguous_headers_are_rejected() {
    let root = std::path::Path::new("/skills/review");
    let (name, skill) = parse::parse("---\r\nname: 'review'\r\ndescription: >-\r\n  Review: code\r\n  and tests\r\nmetadata:\r\n  owner: team\r\n---\r\nbody", root).unwrap();
    assert_eq!(name, "review");
    assert_eq!(skill.description, "Review: code and tests");
    assert_eq!(skill.body, "body");
    for header in [
        "name: review\nname: second\ndescription: x",
        "name: ../escape\ndescription: x",
        "name: review\ndescription: ''",
        "name: review",
        "name: review\ndescription: [a, b]",
    ] {
        assert!(
            parse::parse(&format!("---\n{header}\n---\nbody"), root).is_err(),
            "{header}"
        );
    }
}

#[test]
fn explicit_extra_roots_are_opt_in_and_own_roots_override_them() {
    let rig = Rig::new("skills-roots", Registry::default());
    let rt: rt::Rt = Arc::new(rt::Threads::default());
    let spawn: ext::host::Spawn = Arc::new(move |task| rt.spawn(task));
    let home = rig.dir.join("home");
    let workspace = rig.workspace();
    fs::create_dir_all(&home).unwrap();
    for (dir, body) in [
        (workspace.join(".claude/skills/review"), "external"),
        (home.join("skills/review"), "home"),
        (workspace.join(".a/skills/review"), "workspace"),
    ] {
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: review\ndescription: Review\n---\n{body}"),
        )
        .unwrap();
    }
    let loader = Loader::new(
        &home,
        Skills {
            dirs: vec![".claude/skills".into()],
            ..Skills::default()
        },
    );
    let registry = Registry::default()
        .with(&Arc::new(ext::host::Coordinator::new(
            Registry::default(),
            vec![Arc::new(loader)],
        )))
        .unwrap();
    let source = registry
        .service::<dyn Sources>()
        .unwrap()
        .open(&workspace, spawn.clone(), Arc::new(|_| {}))
        .unwrap();
    let another = registry
        .service::<dyn Sources>()
        .unwrap()
        .open(&workspace, spawn.clone(), Arc::new(|_| {}))
        .unwrap();
    assert!(Arc::ptr_eq(&source, &another));
    drop(another);
    let registry = source.snapshot().registry;
    let catalog = registry.tools().get("skill").unwrap();
    assert_eq!(catalog.replay(), ext::Replay::Unsafe);
    let calls = Rig::new("skills-roots-call", (*registry).clone());
    calls.done("skill", &json!({"name":"review"}));
    assert!(
        calls
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(&e.body, Body::Context{text, ..} if text.ends_with("workspace")))
    );
    let reserved = Loader::new(&home, Skills::default().reserving(["review"]));
    let reserved = Registry::default()
        .with(&Arc::new(ext::host::Coordinator::new(
            Registry::default(),
            vec![Arc::new(reserved)],
        )))
        .unwrap();
    assert!(
        reserved
            .service::<dyn Sources>()
            .unwrap()
            .open(&workspace, spawn.clone(), Arc::new(|_| {}))
            .is_err()
    );
    drop(source);
}
