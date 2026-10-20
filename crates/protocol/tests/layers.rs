//! The layers below the plugins know no plugin: what an extension is for
//! is its own. The protocol, the kernel and the engine are searched for
//! the name of any `ext-*` crate as a string literal, tests aside.

use std::fs;
use std::path::{Path, PathBuf};

/// The crates below the plugin layer.
const BELOW: &[&str] = &["protocol", "kernel", "engine"];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name != "tests" {
                sources(&path, out);
            }
        } else if name.ends_with(".rs") && name != "tests.rs" {
            out.push(path);
        }
    }
}

/// The names of the extensions: every `ext-*` crate but the SDK's derive.
fn plugins(crates: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(crates)
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("ext-")
                .filter(|n| *n != "derive")
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

/// A `kinds!` row names a wire tag, such as the `settings` message (the
/// home settings, no plugin): `Settings "settings" { .. }`.
fn is_wire_tag(line: &str, literal: &str) -> bool {
    let line = line.trim_start();
    line.split_once(' ').is_some_and(|(kind, rest)| {
        kind.starts_with(char::is_uppercase) && rest.starts_with(literal)
    })
}

#[test]
fn no_plugin_is_named_below_the_plugin_layer() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let plugins = plugins(&crates);
    assert!(plugins.contains(&"settings".to_owned()), "{plugins:?}");
    let mut found = Vec::new();
    for krate in BELOW {
        let mut files = Vec::new();
        sources(&crates.join(krate).join("src"), &mut files);
        for file in files {
            let text = fs::read_to_string(&file).unwrap();
            // A file's own unit tests may name what they like.
            let code = text.split("#[cfg(test)]").next().unwrap_or_default();
            for (n, line) in code.lines().enumerate() {
                for plugin in &plugins {
                    let literal = format!("\"{plugin}\"");
                    if line.contains(&literal) && !is_wire_tag(line, &literal) {
                        found.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
                    }
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "a plugin named below the plugin layer:\n{}",
        found.join("\n")
    );
}
