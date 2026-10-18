use std::collections::BTreeMap;

use super::*;

/// A fresh directory holding a home config with `home` and a workspace
/// config with `workspace` (either left out when `None`).
fn dirs(name: &str, home: Option<&str>, workspace: Option<&str>) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("a-config-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let ws = root.join("ws");
    std::fs::create_dir_all(ws.join(".a")).unwrap();
    if let Some(text) = home {
        std::fs::write(root.join("config.toml"), text).unwrap();
    }
    if let Some(text) = workspace {
        std::fs::write(ws.join(".a/config.toml"), text).unwrap();
    }
    (root, ws)
}

fn settings(root: &Path) -> Settings {
    Settings {
        process: |name| (name == "FROM_DAEMON").then(|| "daemon".to_owned()),
        ..Settings::new(root.join("config.toml"), None, None)
    }
}

#[test]
fn files_parse_the_contract() {
    let file: File = r#"
        model = "openai:gpt-6-luna"
        agent = "chat"
        future = 1

        [env]
        OPENAI_API_KEY = "sk-test"
        "#
    .parse()
    .unwrap();
    assert_eq!(
        file,
        File {
            model: Some("openai:gpt-6-luna".into()),
            agent: Some("chat".into()),
            env: BTreeMap::from([("OPENAI_API_KEY".into(), "sk-test".into())]),
        }
    );
}

#[test]
fn later_layers_win() {
    let (root, ws) = dirs(
        "layers",
        Some("model = \"home:m\"\nagent = \"echo\""),
        Some("model = \"ws:m\""),
    );
    let s = settings(&root);
    assert_eq!(s.model(&ws).unwrap(), "ws:m");
    assert_eq!(s.agent(&ws), agent::Kind::Echo);
    assert_eq!(s.model(&root).unwrap(), "home:m");

    let overridden = Settings::new(
        root.join("config.toml"),
        Some("env:m".into()),
        Some(agent::Kind::Chat),
    );
    assert_eq!(overridden.model(&ws).unwrap(), "env:m");
    assert_eq!(overridden.agent(&ws), agent::Kind::Chat);

    let (bare, ws) = dirs("defaults", None, None);
    let s = settings(&bare);
    assert_eq!(s.model(&ws).unwrap(), DEFAULT_MODEL);
    assert_eq!(s.agent(&ws), agent::Kind::Chat);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(bare).unwrap();
}

#[test]
fn variables_come_from_the_client_then_the_home_file_then_the_daemon() {
    let (root, _) = dirs(
        "vars",
        Some("[env]\nFROM_FILE = \"file\"\nSHADOWED = \"file\"\nBLANK = \"file\""),
        // A workspace cannot hand the daemon a key or a base URL.
        Some("[env]\nFROM_WORKSPACE = \"ws\""),
    );
    let s = settings(&root);
    let client = Environment {
        vars: BTreeMap::from([
            ("SHADOWED".into(), "client".into()),
            ("BLANK".into(), " ".into()),
        ]),
        ..Environment::default()
    };
    let var = |name| s.var(Some(&client), name);
    assert_eq!(var("SHADOWED").as_deref(), Some("client"));
    assert_eq!(var("BLANK").as_deref(), Some("file"));
    assert_eq!(var("FROM_FILE").as_deref(), Some("file"));
    assert_eq!(var("FROM_DAEMON").as_deref(), Some("daemon"));
    assert_eq!(var("FROM_WORKSPACE"), None);
    assert_eq!(s.var(None, "SHADOWED").as_deref(), Some("file"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_edited_home_file_applies_without_a_restart() {
    let (root, ws) = dirs("edit", Some("model = \"first:m\""), None);
    let s = settings(&root);
    assert_eq!(s.model(&ws).unwrap(), "first:m");
    std::fs::write(root.join("config.toml"), "model = \"second:model\"").unwrap();
    assert_eq!(s.model(&ws).unwrap(), "second:model");

    std::fs::write(root.join("config.toml"), "model = ").unwrap();
    let error = s.model(&ws).unwrap_err();
    assert!(error.contains("config.toml"), "{error}");
    assert_eq!(s.agent(&ws), agent::Kind::Chat);
    std::fs::remove_dir_all(root).unwrap();
}
