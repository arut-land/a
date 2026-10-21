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
    let s = Settings::looking(root.join("config.toml"), None, Duration::ZERO);
    s.refresh(root);
    s.refresh(&root.join("ws"));
    s
}

#[test]
fn files_parse_the_contract() {
    let file: File = r#"
        model = "openai:gpt-6-luna"
        agent = "loop"
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
            fast_model: None,
            agent: Some("loop".into()),
            env: BTreeMap::from([("OPENAI_API_KEY".into(), "sk-test".into())]),
            daemon: sys::config::Daemon::default(),
            ext: sys::config::Exts::default(),
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
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "ws:m");
    assert_eq!(s.agent(&ws), Some(ext_fixtures::ECHO.to_owned()));
    assert_eq!(s.model(&root).unwrap().0, "home:m");

    let overridden = Settings::looking(
        root.join("config.toml"),
        Some(ext::LOOP.into()),
        Duration::ZERO,
    );
    assert_eq!(overridden.agent(&ws), Some(ext::LOOP.to_owned()));

    let (bare, ws) = dirs("defaults", None, None);
    let s = settings(&bare);
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, DEFAULT_MODEL);
    assert_eq!(s.agent(&ws), None);
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(bare).unwrap();
}

#[test]
fn variables_come_from_the_client_then_the_home_file_never_the_daemon() {
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
    // The daemon's own environment is whichever client's started it.
    assert!(std::env::var_os("PATH").is_some());
    assert_eq!(var("PATH"), None);
    assert_eq!(var("FROM_WORKSPACE"), None);
    assert_eq!(s.var(None, "SHADOWED").as_deref(), Some("file"));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_edited_home_file_applies_without_a_restart() {
    let (root, ws) = dirs("edit", Some("model = \"first:m\""), None);
    let s = settings(&root);
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "first:m");
    std::fs::write(root.join("config.toml"), "model = \"second:model\"").unwrap();
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "second:model");

    std::fs::write(root.join("config.toml"), "model = ").unwrap();
    s.refresh(&ws);
    let error = s.model(&ws).unwrap_err();
    assert!(error.contains("config.toml"), "{error}");
    assert_eq!(s.agent(&ws), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_change_made_through_the_daemon_applies_at_once() {
    let (root, ws) = dirs("change", Some("model = \"aa:m\"\n"), None);
    let s = settings(&root);
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "aa:m");
    let path = root.join("config.toml");
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    let change = SettingsChange {
        model: Some("bb:m".into()),
        ..SettingsChange::default()
    };
    s.home.change(&change).unwrap();
    // As a coarse clock leaves it: the same size and time as before.
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(before)
        .unwrap();
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "bb:m");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_workspace_file_is_parsed_once_per_change_for_all_its_sessions() {
    let (root, ws) = dirs("ws-cache", None, Some("model = \"ws:aa\"\n"));
    let s = settings(&root);
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "ws:aa");
    // The same size and time: what was parsed is used, not read again.
    let path = ws.join(".a/config.toml");
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, "model = \"ws:bb\"\n").unwrap();
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(before).unwrap();
    for _ in 0..3 {
        s.refresh(&ws);
        assert_eq!(s.model(&ws).unwrap().0, "ws:aa");
    }
    // A change is read.
    std::fs::write(&path, "model = \"ws:longer\"\n").unwrap();
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "ws:longer");
    std::fs::remove_dir_all(root).unwrap();
}

/// Between looks the file is trusted, so a call costs no stat: a hand edit
/// waits for the next look, while a change made through the daemon, which
/// is the file's one writer, is seen at once.
#[test]
fn a_file_is_looked_at_once_per_floor_and_the_daemons_own_change_at_once() {
    let (root, ws) = dirs("floor", Some("model = \"first:m\""), None);
    let s = Settings::looking(root.join("config.toml"), None, Duration::from_secs(3600));
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "first:m");
    std::fs::write(root.join("config.toml"), "model = \"second:m\"").unwrap();
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "first:m");
    let change = SettingsChange {
        model: Some("third:m".into()),
        ..SettingsChange::default()
    };
    s.home.change(&change).unwrap();
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "third:m");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn fast_role_routes_through_workspace_then_home_without_changing_the_session_model() {
    let (root, ws) = dirs(
        "side-role",
        Some("model = \"main:m\"\nfast_model = \"home:fast\""),
        Some("fast_model = \"ws:fast\""),
    );
    let s = settings(&root);
    assert_eq!(s.fast_model(&ws).unwrap().as_deref(), Some("ws:fast"));
    s.refresh(&root);
    assert_eq!(s.fast_model(&root).unwrap().as_deref(), Some("home:fast"));
    s.refresh(&ws);
    assert_eq!(s.model(&ws).unwrap().0, "main:m");
    let mut file = File::read(&root.join("config.toml")).unwrap();
    file.model = Some("main:new".into());
    file.write(&root.join("config.toml")).unwrap();
    s.refresh(&root);
    assert_eq!(s.fast_model(&root).unwrap().as_deref(), Some("home:fast"));
    std::fs::remove_dir_all(root).unwrap();
}
