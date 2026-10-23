use super::*;
use std::os::windows::ffi::OsStringExt;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_RoamingAppData, SHGetKnownFolderPath,
};

fn known_folder(id: &windows_sys::core::GUID) -> io::Result<PathBuf> {
    let mut raw = std::ptr::null_mut();
    let result = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw) };
    if result < 0 {
        return Err(io::Error::other(format!(
            "SHGetKnownFolderPath failed: {result:#x}"
        )));
    }
    let mut len = 0;
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
