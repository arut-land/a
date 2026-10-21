//! The apps are renderers over this crate: none derives what a turn is
//! doing, or parses slash commands, itself. Their sources (tests aside)
//! are searched for the signs of a copy.

use std::fs;
use std::path::{Path, PathBuf};

/// What an app holding a rule of its own would write.
const COPIES: &[(&str, &str)] = &[
    (
        ".ledger.activity()",
        "inferring turn completion from an entry prefix",
    ),
    (
        "Approval::Pending",
        "deciding what waits on whom from approvals",
    ),
    (".pending?", "deciding what waits on whom from approvals"),
    ("pendingAsks", "keeping its own list of asks"),
    ("\"waiting for approval\"", "wording a call's status itself"),
    ("\"not approved\"", "wording a call's status itself"),
    ("\"running…\"", "wording a call's status itself"),
    ("var running", "deciding whether a turn runs"),
    ("fn running", "deciding whether a turn runs"),
    (".running()", "deciding whether a turn runs"),
    ("\"/detach\"", "parsing slash commands"),
    ("\"/interrupt\"", "parsing slash commands"),
    ("\"/env refresh\"", "parsing slash commands"),
    ("\"/steer", "parsing slash commands"),
    (
        "part == .outcome",
        "deciding when a reply stops streaming from parts",
    ),
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if !matches!(&*name, "tests" | "Tests" | "Generated" | "build" | ".build") {
                sources(&path, out);
            }
        } else if (name.ends_with(".rs") && name != "tests.rs") || name.ends_with(".swift") {
            out.push(path);
        }
    }
}

#[test]
fn every_app_derives_the_turn_and_parses_commands_through_the_sdk() {
    let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps");
    let mut files = Vec::new();
    sources(&apps, &mut files);
    assert!(files.iter().any(|f| f.ends_with("SessionStore.swift")));
    let mut found = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        // A Rust file's own unit tests may build any state they like.
        let code = text.split("#[cfg(test)]").next().unwrap_or_default();
        for (copy, why) in COPIES {
            if code.contains(copy) {
                found.push(format!("{}: `{copy}` ({why})", file.display()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "rules copied out of the SDK:\n{}",
        found.join("\n")
    );
}
