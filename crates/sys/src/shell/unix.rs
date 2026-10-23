use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::{Dialect, ExecutionFacts};

pub fn find_bash(search: Option<&str>) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    std::iter::once(PathBuf::from("/bin/bash"))
        .chain(
            std::env::split_paths(search.unwrap_or_default()).map(|dir| cwd.join(dir).join("bash")),
        )
        .find(|path| {
            path.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

pub(super) fn discover() -> ExecutionFacts {
    ExecutionFacts {
        dialect: Dialect::Bash,
        // Keep the missing dependency visible at launch; never substitute sh.
        executable: find_bash(std::env::var("PATH").ok().as_deref())
            .unwrap_or_else(|| Path::new("/bin/bash").into()),
        syntax: "Bash (system Bash; no profile or rc files)",
        paths: "POSIX absolute paths; single quotes protect literal paths",
        utilities: if cfg!(target_vendor = "apple") {
            "macOS/BSD utilities; GNU flags are not assumed"
        } else {
            "host PATH utilities; GNU tools are not assumed on minimal systems"
        },
        cleanup: "timeout/cancellation signal the owned process group, TERM then KILL; descendants that leave the group can escape; background work is session-owned; retained output stays in the session spill directory",
        temp: std::env::temp_dir(),
        cwd_command: "pwd -P",
    }
}

pub(super) fn apply_environment(
    facts: &ExecutionFacts,
    env: &mut std::collections::BTreeMap<String, String>,
) {
    env.insert("TMPDIR".into(), facts.temp.to_string_lossy().into_owned());
}

/// Unix arguments are not transported in a Windows command line.
pub fn validate_argv(_: &[String]) -> std::io::Result<()> {
    Ok(())
}
