//! The per-user `Run` key: what Settings > Apps > Startup and Task Manager
//! list and let the user switch off (`StartupApproved`), with no
//! credentials or elevation. A logon task would need COM registration, is
//! invisible in that list, and shows the same console window.

use super::{Home, io};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const NOT_FOUND: u32 = windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;

fn name(home: &Home) -> String {
    format!("a-daemon-{:016x}", crate::endpoint::home_identity(home))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn status(code: u32) -> io::Result<()> {
    match code {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code.cast_signed())),
    }
}

fn system_dir() -> io::Result<PathBuf> {
    let mut buffer = [0u16; 260];
    // SAFETY: the buffer is writable for the length passed.
    let len = unsafe {
        windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(buffer.as_mut_ptr(), 260)
    };
    let len = usize::try_from(len).map_err(|_| io::ErrorKind::InvalidData)?;
    if len == 0 || len >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(OsString::from_wide(&buffer[..len]).into())
}

/// A console program started at logon gets a console window (a Terminal
/// tab on Windows 11); a headless console host gives it none.
fn command(home: &Home) -> io::Result<String> {
    let host = system_dir()?.join("conhost.exe");
    let exe = home.runtime().current_exe();
    let (Some(host), Some(exe)) = (host.to_str(), exe.to_str()) else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    // Windows paths cannot contain quotes, so quoting cannot be escaped.
    Ok(format!("\"{host}\" --headless \"{exe}\" __daemon --login"))
}

fn read(name: &str) -> io::Result<Option<String>> {
    let mut buffer = vec![0u16; 2048];
    let mut bytes = u32::try_from(buffer.len() * 2).expect("small buffer");
    let (key, value) = (wide(RUN), wide(name));
    // SAFETY: terminated key/value names and a writable, size-described buffer.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &raw mut bytes,
        )
    };
    if code == NOT_FOUND {
        return Ok(None);
    }
    status(code)?;
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Ok(Some(String::from_utf16_lossy(&buffer[..len])))
}

fn write(name: &str, command: &str) -> io::Result<()> {
    let (key, value, data) = (wide(RUN), wide(name), wide(command));
    // SAFETY: terminated strings; the data length covers its terminator.
    status(unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            REG_SZ,
            data.as_ptr().cast(),
            u32::try_from(data.len() * 2).map_err(|_| io::ErrorKind::InvalidInput)?,
        )
    })
}

fn delete(key: &str, name: &str) -> io::Result<()> {
    let (key, value) = (wide(key), wide(name));
    // SAFETY: terminated key and value names.
    match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) } {
        NOT_FOUND => Ok(()),
        code => status(code),
    }
}

/// Explorer records the user's switch as binary data whose first byte is
/// even when the entry may run; no record means it may.
fn approved(name: &str) -> bool {
    let mut data = [0u8; 12];
    let mut bytes = 12u32;
    let (key, value) = (wide(APPROVED), wide(name));
    // SAFETY: terminated names and a writable, size-described buffer.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &raw mut bytes,
        )
    };
    code != 0 || bytes == 0 || data[0] & 1 == 0
}

pub(super) fn enable(home: &Home) -> io::Result<()> {
    write(&name(home), &command(home)?)
}

pub(super) fn disable(home: &Home) -> io::Result<()> {
    let name = name(home);
    delete(RUN, &name)?;
    delete(APPROVED, &name)
}

pub(super) fn refresh(home: &Home) -> io::Result<bool> {
    let name = name(home);
    let Some(current) = read(&name)? else {
        return Ok(false);
    };
    let command = command(home)?;
    if current != command {
        write(&name, &command)?;
    }
    Ok(approved(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_names_cannot_cross_state_homes() {
        let root = std::env::temp_dir().join(format!("a-autostart-names-{}", std::process::id()));
        assert_ne!(
            name(&Home::from(root.join("one"))),
            name(&Home::from(root.join("two")))
        );
    }

    #[test]
    fn the_logon_entry_runs_the_daemon_without_a_console_window() {
        let command = command(&Home::from(std::env::temp_dir().join("a-autostart-run"))).unwrap();
        assert!(command.starts_with('"'));
        assert!(command.contains("conhost.exe\" --headless \""));
        assert!(command.ends_with("\" __daemon --login"));
    }
}
