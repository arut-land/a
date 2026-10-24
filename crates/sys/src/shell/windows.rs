use std::io::Read;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6432KEY,
    RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};

use super::{Dialect, ExecutionFacts};

fn installed_git() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let key: Vec<u16> = "Software\\GitForWindows\0".encode_utf16().collect();
    let value: Vec<u16> = "InstallPath\0".encode_utf16().collect();
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for view in [RRF_SUBKEY_WOW6464KEY, RRF_SUBKEY_WOW6432KEY] {
            let mut buffer = vec![0u16; 32768];
            let mut bytes = 65536u32;
            // SAFETY: terminated key/value strings and a writable, size-described buffer.
            let status = unsafe {
                RegGetValueW(
                    hive,
                    key.as_ptr(),
                    value.as_ptr(),
                    RRF_RT_REG_SZ | view,
                    std::ptr::null_mut(),
                    buffer.as_mut_ptr().cast(),
                    &raw mut bytes,
                )
            };
            if status == 0 {
                let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
                roots.push(std::ffi::OsString::from_wide(&buffer[..len]).into());
            }
        }
    }
    roots
}

fn path_candidates(search: Option<&str>, name: &str) -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    std::env::split_paths(search.unwrap_or_default())
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| cwd.join(dir).join(name))
        .collect()
}

fn native_executable(path: &Path) -> bool {
    let mut header = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .is_ok()
        && header == *b"MZ"
}

fn first_bash(candidates: impl IntoIterator<Item = PathBuf>, windows: &Path) -> Option<PathBuf> {
    let system = ["System32", "SysWOW64", "Sysnative"].map(|dir| windows.join(dir));
    candidates
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .find(|path| {
            // Canonicalize first so a PATH alias cannot admit the WSL launcher.
            !system.iter().any(|dir| {
                path.to_string_lossy().to_lowercase().starts_with(&format!(
                    "{}\\",
                    dir.canonicalize()
                        .unwrap_or_else(|_| dir.clone())
                        .to_string_lossy()
                        .to_lowercase()
                ))
            }) && native_executable(path)
        })
}

pub fn find_bash(search: Option<&str>) -> Option<PathBuf> {
    let mut candidates = path_candidates(search, "bash.exe");
    let mut roots = installed_git();
    for variable in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(variable) {
            roots.push(PathBuf::from(root).join("Git"));
        }
    }
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(root).join("Programs/Git"));
    }
    let drive = std::env::var_os("SystemDrive").unwrap_or_else(|| "C:".into());
    for name in ["msys64", "msys32"] {
        roots.push(PathBuf::from(format!(
            "{}\\{name}",
            drive.to_string_lossy()
        )));
    }
    for root in roots {
        candidates.extend([root.join("bin/bash.exe"), root.join("usr/bin/bash.exe")]);
    }
    let windows =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    first_bash(candidates, &windows)
}

fn powershell(
    search: Option<&str>,
    program_files: Option<&Path>,
    windows: &Path,
) -> (PathBuf, &'static str) {
    let mut candidates = path_candidates(search, "pwsh.exe");
    if let Some(root) = program_files {
        candidates.push(root.join("PowerShell/7/pwsh.exe"));
    }
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        (path, "PowerShell (pwsh; no profile; noninteractive)")
    } else {
        windows_powershell(windows)
    }
}

fn windows_powershell(windows: &Path) -> (PathBuf, &'static str) {
    (
        windows.join("System32/WindowsPowerShell/v1.0/powershell.exe"),
        "Windows PowerShell 5.1 (no profile; noninteractive)",
    )
}

pub(super) fn discover(temp: PathBuf) -> ExecutionFacts {
    let search = std::env::var("PATH").ok();
    let bash = find_bash(search.as_deref());
    let windows =
        std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    let program_files = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from);
    resolve(
        bash,
        search.as_deref(),
        program_files.as_deref(),
        &windows,
        temp,
    )
}

fn resolve(
    bash: Option<PathBuf>,
    search: Option<&str>,
    program_files: Option<&Path>,
    windows: &Path,
    temp: PathBuf,
) -> ExecutionFacts {
    let (dialect, executable, mut syntax, paths) = if let Some(path) = bash {
        (
            Dialect::Bash,
            path,
            "Bash (native Windows runtime; no profile or rc files)",
            "native Bash paths: C:/dir and runtime POSIX paths; native tools use Windows drive/UNC paths",
        )
    } else {
        let (path, syntax) = powershell(search, program_files, windows);
        (
            if path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("powershell.exe"))
            {
                Dialect::WindowsPowerShell
            } else {
                Dialect::PowerShell
            },
            path,
            syntax,
            "Windows drive and UNC paths; use -LiteralPath; single quotes escape apostrophes by doubling them",
        )
    };
    let mut runtime_bin = None;
    let cwd_command = if dialect == Dialect::Bash {
        let parent = executable.parent().unwrap_or(Path::new("."));
        if parent.join("cygwin1.dll").is_file() {
            runtime_bin = Some(parent.to_path_buf());
            syntax = "Cygwin Bash (no profile or rc files)";
            "cygpath -w -- \"$PWD\""
        } else if parent.join("msys-2.0.dll").is_file()
            || parent.join("../usr/bin/msys-2.0.dll").is_file()
        {
            runtime_bin = Some(if parent.join("msys-2.0.dll").is_file() {
                parent.to_path_buf()
            } else {
                parent.join("../usr/bin")
            });
            syntax = "MSYS/Git Bash (no profile or rc files)";
            "pwd -W"
        } else {
            syntax = "unsupported native Bash runtime (expected MSYS/Git or Cygwin)";
            ""
        }
    } else {
        ""
    };
    ExecutionFacts {
        dialect,
        executable,
        syntax,
        paths,
        utilities: "installed Windows executables and the selected shell's builtins",
        cleanup: "timeout/cancellation terminate the owned Windows job and its descendants; no Unix TERM grace; background work is session-owned; jobs die with the daemon; cut output is kept whole in the daemon's blob store; command-created temporary files are not automatically removed",
        temp,
        cwd_command,
        runtime_bin,
    }
}

pub(super) fn apply_environment(
    facts: &ExecutionFacts,
    env: &mut std::collections::BTreeMap<String, String>,
) {
    if let Some(bin) = &facts.runtime_bin {
        let old = env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
            .map_or_else(String::new, |(_, value)| value.clone());
        let mut paths: Vec<_> = std::env::split_paths(&old).collect();
        if !paths.iter().any(|path| path == bin) {
            paths.push(bin.clone());
        }
        if let Ok(path) = std::env::join_paths(paths) {
            env.retain(|key, _| !key.eq_ignore_ascii_case("PATH"));
            env.insert("PATH".into(), path.to_string_lossy().into_owned());
        }
    }
    // Windows environment names compare without case; remove older spellings
    // before inserting the frozen temporary-directory values.
    env.retain(|key, _| {
        !["TEMP", "TMP", "TMPDIR", "BASH_ENV", "ENV"]
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name))
    });
    for key in ["TEMP", "TMP", "TMPDIR"] {
        env.insert(key.into(), facts.temp.to_string_lossy().into_owned());
    }
}

/// Check the complete native command line with the same quoting as `SpawnPlan`.
pub fn validate_argv(argv: &[String]) -> std::io::Result<()> {
    let mut units = 0usize;
    for arg in argv {
        let arg: Vec<_> = arg.encode_utf16().collect();
        if arg.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "shell argument contains NUL",
            ));
        }
        // One separator per argument also accounts for the final NUL.
        units = units.saturating_add(crate::windows_quote::quote(&arg).len() + 1);
    }
    if units > 32767 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "shell command exceeds Windows' 32,767 UTF-16 command-line limit after native quoting",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_bash_is_never_a_native_shell_candidate() {
        let root = std::env::temp_dir().join(format!("a-shell-discovery-{}", std::process::id()));
        let system = root.join("System32");
        let git = root.join("Git/usr/bin");
        std::fs::create_dir_all(&system).unwrap();
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(system.join("bash.exe"), b"MZ").unwrap();
        std::fs::write(git.join("bash.exe"), b"MZ").unwrap();
        assert_eq!(
            first_bash([system.join("bash.exe"), git.join("bash.exe")], &root),
            Some(git.join("bash.exe").canonicalize().unwrap())
        );
        assert!(first_bash([system.join("bash.exe")], &root).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_bash_selects_only_the_powershell_identity() {
        let facts = resolve(
            None,
            Some(""),
            None,
            Path::new(r"C:\Windows"),
            r"C:\startup-temp".into(),
        );
        assert_eq!(facts.dialect, Dialect::WindowsPowerShell);
        assert_eq!(facts.tool(), "powershell");
        assert!(facts.syntax.contains("5.1"));
    }

    #[test]
    fn absent_pwsh_selects_windows_powershell() {
        let windows = Path::new(r"C:\Windows");
        let (path, syntax) = powershell(Some(""), None, windows);
        assert_eq!(
            path,
            windows.join("System32/WindowsPowerShell/v1.0/powershell.exe")
        );
        assert!(syntax.contains("5.1"));
    }

    #[test]
    fn native_quoting_expansion_counts_toward_the_command_line_limit() {
        assert!(validate_argv(&["pwsh.exe".into(), "\"".repeat(20_000)]).is_err());
        assert!(validate_argv(&["pwsh.exe".into(), "\"".repeat(10_000)]).is_ok());
    }

    #[test]
    fn shell_environment_preserves_client_path_priority_and_pins_temp() {
        let mut facts = resolve(
            None,
            Some(""),
            None,
            Path::new(r"C:\Windows"),
            r"C:\startup-temp".into(),
        );
        facts.runtime_bin = Some(r"C:\Git\usr\bin".into());
        let mut env = std::collections::BTreeMap::from([
            ("Path".into(), r"C:\client".into()),
            ("tmp".into(), r"C:\later-temp".into()),
            ("bash_env".into(), "unwanted".into()),
        ]);
        apply_environment(&facts, &mut env);
        assert_eq!(
            std::env::split_paths(&env["PATH"]).collect::<Vec<_>>(),
            [
                PathBuf::from(r"C:\client"),
                PathBuf::from(r"C:\Git\usr\bin")
            ]
        );
        assert_eq!(env["TEMP"], r"C:\startup-temp");
        assert_eq!(env["TMP"], r"C:\startup-temp");
        assert!(!env.contains_key("Path"));
        assert!(!env.contains_key("tmp"));
        assert!(!env.contains_key("bash_env"));
    }
}
