//! The extension contract is I/O-free: the kernel builds on it, so what it
//! pulls in the kernel would too. A call reaches the OS or a runtime only
//! through the services the engine provides.

use std::path::Path;
use std::process::Command;

#[test]
fn the_sdk_does_no_io() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tree = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "ext",
            "-e",
            "normal",
            "--prefix",
            "none",
            "-f",
            "{p}",
            "--offline",
        ])
        .current_dir(manifest)
        .output()
        .unwrap();
    assert!(
        tree.status.success(),
        "{}",
        String::from_utf8_lossy(&tree.stderr)
    );
    let tree = String::from_utf8(tree.stdout).unwrap();
    let crates: Vec<&str> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert!(crates.contains(&"protocol"), "{tree}");
    for io in [
        "rt",
        "sys",
        "engine",
        "kernel",
        "ureq",
        "mio",
        "rustix",
        "tokio",
        "smol",
        "async-task",
        "async-io",
        "async-executor",
        "polling",
    ] {
        assert!(!crates.contains(&io), "ext pulls in {io}:\n{tree}");
    }
}
