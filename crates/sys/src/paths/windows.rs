use super::{Command, InstallLayout, Locations, PathBuf, Paths, io};
use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library};
use std::os::windows::ffi::OsStringExt;
use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;

type KnownFolder = unsafe extern "system" fn(
    *const windows_sys::core::GUID,
    u32,
    *mut std::ffi::c_void,
    *mut *mut u16,
) -> i32;
type Free = unsafe extern "system" fn(*mut std::ffi::c_void);

fn known_folder(id: &windows_sys::core::GUID) -> io::Result<PathBuf> {
    // The API contract preserves redirection without importing Shell32 into every
    // process. Each loader reference ends after the returned path is copied.
    // SAFETY: system-only search selects Windows API contracts. Their initialization
    // and termination require no application-owned callbacks or retained objects.
    let (folders, com) = unsafe {
        (
            Library::load_with_flags(
                "api-ms-win-shell-shellfolders-l1-1-0.dll",
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            ),
            Library::load_with_flags(
                "api-ms-win-core-com-l1-1-0.dll",
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            ),
        )
    };
    let folders = folders.map_err(io::Error::other)?;
    let com = com.map_err(io::Error::other)?;
    // SAFETY: the symbol types match the documented Windows ABI. Both libraries
    // outlive these calls and the allocation is freed before either is released.
    let (resolve, free) = unsafe {
        (
            folders.get::<KnownFolder>(b"SHGetKnownFolderPath\0"),
            com.get::<Free>(b"CoTaskMemFree\0"),
        )
    };
    let resolve = resolve.map_err(io::Error::other)?;
    let free = free.map_err(io::Error::other)?;
    let mut raw = std::ptr::null_mut();
    // SAFETY: id is a valid GUID and raw is a writable output pointer. A null
    // token selects the current user.
    let result = unsafe { resolve(id, 0, std::ptr::null_mut(), &raw mut raw) };
    if result < 0 {
        // SAFETY: the API owns this allocation; CoTaskMemFree accepts null.
        unsafe { free(raw.cast()) };
        return Err(io::Error::other(format!(
            "SHGetKnownFolderPath failed: {result:#x}"
        )));
    }
    let mut len = 0;
    // SAFETY: a successful call returns a NUL-terminated wide string allocated
    // by COM. Copy it before freeing the allocation with its matching allocator.
    unsafe {
        while *raw.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
            raw, len,
        )));
        free(raw.cast());
        Ok(path.join("a"))
    }
}

pub(super) fn resolve() -> io::Result<Paths> {
    let local = known_folder(&FOLDERID_LocalAppData)?;
    Ok(Paths::new(Locations {
        layout: InstallLayout::Native,
        // Provider keys and machine-specific tool paths are not roaming preferences.
        config: local.join("config"),
        data: local.join("data"),
        state: local.join("state"),
        cache: local.join("cache"),
        logs: local.join("logs"),
        temporary: local.join("temp"),
        runtime: local.join("state/run"),
    }))
}

pub(super) fn daemon_env(_: &Paths, _: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_paths_do_not_retain_shell_or_com_modules() {
        const CHILD: &str = "A_TEST_PATHS_MODULES";
        if std::env::var_os(CHILD).is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "paths::native::tests::native_paths_do_not_retain_shell_or_com_modules",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let paths = resolve().unwrap();
        assert!(paths.config_dir().is_absolute());
        for name in ["shell32.dll", "ole32.dll", "combase.dll"] {
            assert!(
                Library::open_already_loaded(name).is_err(),
                "retained {name}"
            );
        }
    }

    #[test]
    fn credentials_and_machine_state_never_use_roaming_storage() {
        let paths = resolve().unwrap();
        let local = known_folder(&FOLDERID_LocalAppData).unwrap();
        assert_eq!(paths.config_dir(), local.join("config"));
        for path in [
            paths.config_dir(),
            paths.state_dir(),
            paths.data_dir(),
            paths.cache_dir(),
            paths.temporary_dir(),
            paths.runtime_dir(),
            &paths.logs_dir(),
        ] {
            assert!(path.starts_with(&local));
        }
        assert!(paths.spill_dir().starts_with(paths.state_dir()));
    }
}
