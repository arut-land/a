//! Owned Windows resources and the completion events awaited by `rt`.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

pub fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

pub fn ok(success: i32) -> io::Result<()> {
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the caller transfers a newly returned, checked handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

pub fn event() -> io::Result<OwnedHandle> {
    // SAFETY: no name or inherited security; a manual-reset completion event.
    owned(unsafe { CreateEventW(null(), 1, 0, null()) })
}

/// Each direction has its own completion event; pipe handles themselves
/// cannot be registered as readiness sources on Windows.
pub trait CompletionSource {
    fn completion_events(&self) -> io::Result<[OwnedHandle; 2]>;
    fn nonblocking(&self) -> io::Result<()>;
    fn complete_write(&self) -> io::Result<()> {
        Ok(())
    }
}

impl<T: CompletionSource> CompletionSource for &T {
    fn completion_events(&self) -> io::Result<[OwnedHandle; 2]> {
        T::completion_events(self)
    }
    fn nonblocking(&self) -> io::Result<()> {
        T::nonblocking(self)
    }
    fn complete_write(&self) -> io::Result<()> {
        T::complete_write(self)
    }
}

pub fn user_sid(pid: Option<u32>) -> io::Result<String> {
    let process = pid
        .map(|pid| {
            // SAFETY: scalar inputs; the returned handle is checked and owned.
            owned(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })
        })
        .transpose()?;
    // SAFETY: the pseudo-handle needs no close; an opened process lives through this call.
    let process = process.as_ref().map_or_else(
        || unsafe { GetCurrentProcess() },
        AsRawHandle::as_raw_handle,
    );
    let mut token = null_mut();
    // SAFETY: valid process and writable handle output.
    ok(unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) })?;
    let token = owned(token)?;
    let mut size = 0;
    // SAFETY: a null buffer queries the required byte count.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            null_mut(),
            0,
            &raw mut size,
        )
    };
    // usize storage keeps TOKEN_USER and its embedded SID suitably aligned.
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    // SAFETY: the allocated buffer is at least `size` bytes and aligned.
    ok(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &raw mut size,
        )
    })?;
    // SAFETY: successful TokenUser filled this aligned buffer, including the SID.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = null_mut();
    // SAFETY: the SID remains alive in buffer; the output is a LocalAlloc string.
    ok(unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut text) })?;
    let mut len = 0;
    // SAFETY: the successful conversion returned a NUL-terminated string.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` covers the string and LocalFree matches its allocator.
    let sid = unsafe {
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        sid
    };
    Ok(sid)
}

pub struct Security(*mut std::ffi::c_void);

impl Security {
    pub fn user_only() -> io::Result<Self> {
        let sddl = wide(OsStr::new(&format!("D:P(A;;GA;;;{})", user_sid(None)?)));
        let mut descriptor = null_mut();
        // SAFETY: NUL-terminated SDDL and writable output; revision 1.
        ok(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &raw mut descriptor,
                null_mut(),
            )
        })?;
        Ok(Self(descriptor))
    }

    pub fn descriptor(&self) -> *mut std::ffi::c_void {
        self.0
    }

    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap(),
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: this descriptor was allocated by the SDDL conversion.
        unsafe { LocalFree(self.0) };
    }
}

pub fn duplicate(handle: &impl AsHandle) -> io::Result<OwnedHandle> {
    handle.as_handle().try_clone_to_owned()
}
