use super::{Command, InstallLayout, PathBuf, Paths, io};
use std::os::windows::ffi::OsStringExt;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_RoamingAppData, SHGetKnownFolderPath,
};

fn known_folder(id: &windows_sys::core::GUID) -> io::Result<PathBuf> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: id is a valid GUID and raw is a writable output pointer. A null
    // token selects the current user and requires no COM initialization.
    let result = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw) };
    if result < 0 {
        // SAFETY: the API owns this allocation; CoTaskMemFree accepts null.
        unsafe { CoTaskMemFree(raw.cast()) };
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
        CoTaskMemFree(raw.cast());
        Ok(path.join("a"))
    }
}

pub(super) fn resolve() -> io::Result<Paths> {
    let local = known_folder(&FOLDERID_LocalAppData)?;
    Ok(Paths {
        layout: InstallLayout::Native,
        config: known_folder(&FOLDERID_RoamingAppData)?,
        data: local.join("data"),
        state: local.join("state"),
        cache: local.join("cache"),
        logs: local.join("logs"),
        temporary: local.join("temp"),
        runtime: local.join("state/run"),
    })
}

pub(super) fn daemon_env(_: &Paths, _: &mut Command) {}

pub(super) fn remote_runtime_script() -> &'static str {
    // Native Windows bootstrap is not a POSIX/uname protocol. Connections
    // still work through an explicitly installed `a` on the remote PATH.
    "exit 127"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_user_configuration_roams() {
        let paths = resolve().unwrap();
        let local = known_folder(&FOLDERID_LocalAppData).unwrap();
        assert_eq!(
            paths.config_dir(),
            known_folder(&FOLDERID_RoamingAppData).unwrap()
        );
        for path in [
            paths.state_dir(),
            paths.data_dir(),
            paths.cache_dir(),
            paths.temporary_dir(),
            paths.runtime_dir(),
            &paths.logs_dir(),
        ] {
            assert!(path.starts_with(&local));
            assert_ne!(path, paths.config_dir());
        }
        assert!(paths.spill_dir().starts_with(paths.state_dir()));
    }
}
