//! The per-user `Run` key: what Settings > Apps > Startup and Task Manager
//! list and let the user switch off (`StartupApproved`), with no
//! credentials or elevation. A logon task would need COM registration, is
//! invisible in that list, and gets the same console allocation.

use super::{Home, io};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};

pub(super) const LOGIN: super::Login = super::Login::Spawn;

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const NOT_FOUND: u32 = windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
const MORE_DATA: u32 = windows_sys::Win32::Foundation::ERROR_MORE_DATA;

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

/// `a.exe`'s manifest asks for no console, so the entry opens no window
/// where the policy exists; older releases show one only while the entry
/// starts a detached daemon and exits ([`super::Login::Spawn`]).
fn command(home: &Home) -> io::Result<String> {
    let exe = home.runtime().current_exe();
    let exe = exe.to_str().ok_or(io::ErrorKind::InvalidInput)?;
    // Windows paths cannot contain quotes, so quoting cannot be escaped.
    Ok(format!("\"{exe}\" __daemon --login"))
}

fn read(name: &str) -> io::Result<Option<String>> {
    let (key, value) = (wide(RUN), wide(name));
    let query = |buffer: *mut u16, bytes: &mut u32| {
        // SAFETY: terminated key/value names; a null buffer asks only for the
        // size, otherwise the buffer holds `bytes`.
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.cast(),
                bytes,
            )
        }
    };
    // Another program may rewrite the value between the two calls: ask again
    // for the size it has now.
    loop {
        let mut bytes = 0u32;
        match query(std::ptr::null_mut(), &mut bytes) {
            NOT_FOUND => return Ok(None),
            code => status(code)?,
        }
        let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
        match query(buffer.as_mut_ptr(), &mut bytes) {
            NOT_FOUND => return Ok(None),
            MORE_DATA => {}
            code => {
                status(code)?;
                let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
                return Ok(Some(String::from_utf16_lossy(&buffer[..len])));
            }
        }
    }
}

/// Run commands are limited to 260 UTF-16 code units, excluding the NUL.
/// <https://learn.microsoft.com/windows/win32/setupapi/run-and-runonce-registry-keys>
fn write(
    name: &str,
    command: &str,
    set: impl FnOnce(&str, &[u16]) -> io::Result<()>,
) -> io::Result<()> {
    let data = wide(command);
    if data.len() - 1 > 260 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the installed daemon command exceeds the Windows Run limit of 260 UTF-16 code units; install a under a shorter user profile path",
        ));
    }
    set(name, &data)
}

fn set(name: &str, data: &[u16]) -> io::Result<()> {
    let (key, value) = (wide(RUN), wide(name));
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

pub(super) fn registration(home: &Home) -> io::Result<super::Registration> {
    let name = name(home);
    Ok(match read(&name)? {
        None => super::Registration::Unregistered,
        Some(_) if approved(&name) => super::Registration::Enabled,
        Some(_) => super::Registration::Disabled,
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
    write(&name(home), &command(home)?, set)
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
        write(&name, &command, set)?;
    }
    Ok(approved(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_commands_are_bounded_in_utf16_before_registry_mutation() {
        for command in ["a".repeat(260), "🦀".repeat(130)] {
            write("test", &command, |name, data| {
                assert_eq!(name, "test");
                assert_eq!(data.len(), 261);
                assert_eq!(data.last(), Some(&0));
                Ok(())
            })
            .unwrap();
        }
        for command in [
            "a".repeat(261),
            format!("{}a", "🦀".repeat(130)),
            "🦀".repeat(131),
        ] {
            assert_eq!(
                write("test", &command, |_, _| panic!(
                    "unsupported command reached the registry"
                ))
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidInput,
            );
        }
    }

    #[test]
    fn registration_names_cannot_cross_state_homes() {
        let root = std::env::temp_dir().join(format!("a-autostart-names-{}", std::process::id()));
        assert_ne!(
            name(&Home::from(root.join("one"))),
            name(&Home::from(root.join("two")))
        );
    }

    #[test]
    fn the_logon_entry_runs_the_installed_daemon() {
        let command = command(&Home::from(std::env::temp_dir().join("a-autostart-run"))).unwrap();
        assert!(command.starts_with('"') && command.ends_with("a.exe\" __daemon --login"));
    }
}
