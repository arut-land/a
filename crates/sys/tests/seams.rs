//! `sys` (and `rt` under it) is where platforms differ. The crates above
//! it are written once, for every platform, against its seams.

use std::fs;
use std::path::{Path, PathBuf};

/// Include renderers so an app cannot hide a platform decision from the core scan.
fn portable_crates() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut dirs = Vec::new();
    for parent in ["crates", "apps"] {
        for item in fs::read_dir(root.join(parent)).unwrap() {
            let path = item.unwrap().path();
            if path.join("src").is_dir()
                && !matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("sys" | "rt")
                )
            {
                dirs.push(path);
            }
        }
    }
    dirs.sort();
    dirs
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Platform decisions and native operations belong behind the same seam.
fn names_a_platform(line: &str) -> bool {
    const PLATFORM: [&str; 7] = [
        "unix",
        "windows",
        "target_os",
        "target_arch",
        "target_family",
        "target_vendor",
        "target_env",
    ];
    let line = line.trim_start();
    if line.starts_with("//") {
        return false;
    }
    ["env::consts", "std::os::", "rustix::", "libc::"]
        .iter()
        .any(|native| line.contains(native))
        || ["cfg(", "cfg!(", "cfg_attr("].iter().any(|cfg| {
            line.split(cfg)
                .skip(1)
                .any(|rest| PLATFORM.iter().any(|p| rest.contains(p)))
        })
}

/// Whether `line` gates what follows to tests: test code may check what a
/// platform does (file modes, say) without a seam of its own.
fn gates_tests(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test,")
}

/// The platform conditions in `source`, as `line: text`. Test files and
/// test modules (which close a file, by convention) are allowed theirs.
fn platform_cfgs(path: &Path, source: &str) -> Vec<String> {
    let name = path.file_name().unwrap().to_str().unwrap();
    let in_tests_dir = path.components().any(|c| c.as_os_str() == "tests");
    if in_tests_dir || name == "tests.rs" || name.ends_with("_tests.rs") {
        return Vec::new();
    }
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        let next = lines.get(n + 1).map_or("", |l| l.trim_start());
        if gates_tests(line) && (next.starts_with("mod ") || next.starts_with("pub mod ")) {
            if next.contains('{') {
                break;
            }
            continue;
        }
        if names_a_platform(line) {
            found.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
        }
    }
    found
}

#[test]
fn production_code_above_the_seams_is_platform_independent() {
    let mut files = Vec::new();
    for dir in portable_crates() {
        sources(&dir.join("src"), &mut files);
    }
    assert!(files.len() > 20, "found the sources: {files:?}");
    let found: Vec<String> = files
        .iter()
        .flat_map(|path| platform_cfgs(path, &fs::read_to_string(path).unwrap()))
        .collect();
    assert_eq!(
        found,
        [""; 0],
        "platform operations belong behind a seam in `sys` or `rt`:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_scan_catches_native_lookups_and_allows_test_modules() {
    let path = Path::new("crates/x/src/lib.rs");
    let source = "\
#[cfg(unix)]
fn a() {}
// #[cfg(windows)] is only mentioned
const B: bool = cfg!(target_os = \"linux\");
#[cfg_attr(windows, allow(dead_code))]
#[cfg(feature = \"web\")]
fn c() {}
#[cfg(test)]
mod early_tests;
const OS: &str = std::env::consts::OS;
use std::os::fd::BorrowedFd;
#[cfg(all(test, unix))]
mod tests {
#[cfg(unix)]
fn d() {}
}
";
    let found = platform_cfgs(path, source);
    assert_eq!(found.len(), 5, "{found:?}");
    assert!(found[0].ends_with(":1: #[cfg(unix)]"));
    assert!(found[1].contains(":4: "));
    assert!(found[2].contains(":5: "));
    for test_file in ["crates/x/src/a/tests.rs", "crates/x/tests/t.rs"] {
        assert_eq!(platform_cfgs(Path::new(test_file), source), [""; 0]);
    }
}
