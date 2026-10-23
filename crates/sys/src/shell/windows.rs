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
            let mut buffer = [0u16; 32768];
            let mut bytes = std::mem::size_of_val(&buffer) as u32;
            // SAFETY: terminated key/value strings and a writable, size-described buffer.
            let status = unsafe {
                RegGetValueW(
                    hive,
                    key.as_ptr(),
                    value.as_ptr(),
                    RRF_RT_REG_SZ | view,
                    std::ptr::null_mut(),
                    buffer.as_mut_ptr().cast(),
                    &mut bytes,
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
    let windows = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
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
    if let Some(path) = candidates.into_iter().find(|path| native_executable(path)) {
        (path, "PowerShell (pwsh; no profile; noninteractive)")
    } else {
        (
            windows.join("System32/WindowsPowerShell/v1.0/powershell.exe"),
            "Windows PowerShell 5.1 (no profile; noninteractive)",
        )
    }
}

pub(super) fn discover() -> ExecutionFacts {
    let search = std::env::var("PATH").ok();
    let bash = find_bash(search.as_deref());
    let windows = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let program_files = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from);
    let (dialect, executable, syntax, paths) = match bash {
        Some(path) => (
            Dialect::Bash,
            path,
            "Bash (native Windows runtime; no profile or rc files)",
            "MSYS/Git Bash paths: C:/dir or /c/dir; native tools use Windows paths",
        ),
        None => {
            let (path, syntax) = powershell(search.as_deref(), program_files.as_deref(), &windows);
            (
                Dialect::PowerShell,
                path,
                syntax,
                "Windows drive and UNC paths; use -LiteralPath; single quotes escape apostrophes by doubling them",
            )
        }
    };
    ExecutionFacts {
        dialect,
        executable,
        syntax,
        paths,
        utilities: "installed Windows executables and the selected shell's builtins",
        cleanup: "timeout/cancellation terminate the owned Windows job and its descendants; no Unix TERM grace; background work is session-owned; jobs die with the daemon; retained output stays in the session spill directory",
        temp: std::env::temp_dir(),
    }
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
    fn absent_pwsh_selects_windows_powershell() {
        let windows = Path::new(r"C:\Windows");
        let (path, syntax) = powershell(Some(""), None, windows);
        assert_eq!(
            path,
            windows.join("System32/WindowsPowerShell/v1.0/powershell.exe")
        );
        assert!(syntax.contains("5.1"));
    }
}
