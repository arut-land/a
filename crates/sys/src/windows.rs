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
    ACL, EqualSid, GetSecurityDescriptorDacl, GetTokenInformation, PSID, SECURITY_ATTRIBUTES,
    TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER, TokenGroups, TokenUser,
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
    let user = token_information(process.as_ref(), TokenUser)?;
    // SAFETY: a successful TokenUser query filled this aligned buffer, SID included.
    sid_string(unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid })
}

/// Who this process is when judging an existing object's owner.
pub struct Identity {
    user: Vec<usize>,
    /// The token holds BUILTIN\Administrators, even deny-only: an elevated
    /// run of the same user gives its objects to that group.
    administrator: bool,
}

const ADMINISTRATORS: &str = "S-1-5-32-544";

impl Identity {
    pub fn current() -> io::Result<Self> {
        let groups = token_information(None, TokenGroups)?;
        // SAFETY: a successful TokenGroups query filled this aligned buffer
        // with `GroupCount` entries, each SID within it.
        let groups = unsafe {
            let groups = &*groups.as_ptr().cast::<TOKEN_GROUPS>();
            std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize)
                .iter()
                .map(|group| sid_string(group.Sid))
                .collect::<io::Result<Vec<_>>>()?
        };
        Ok(Self {
            user: token_information(None, TokenUser)?,
            administrator: groups.iter().any(|sid| sid == ADMINISTRATORS),
        })
    }

    pub fn user(&self) -> PSID {
        // SAFETY: the buffer holds a successful TokenUser query.
        unsafe { (*self.user.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    /// Whether an object owned by `sid` is this user's: owned by the user,
    /// or by Administrators when the user is one, as Git's `safe.directory`
    /// accepts, so one elevated run does not lock the user out.
    pub fn owns(&self, sid: PSID) -> io::Result<bool> {
        if sid.is_null() {
            return Ok(false);
        }
        // SAFETY: both SIDs are valid and outlive the comparison.
        if unsafe { EqualSid(sid, self.user()) } != 0 {
            return Ok(true);
        }
        Ok(self.administrator && sid_string(sid)? == ADMINISTRATORS)
    }
}

/// One token information class for `process`, or this process when `None`.
/// usize storage keeps the structure and the SIDs embedded in it aligned.
fn token_information(
    process: Option<&OwnedHandle>,
    class: TOKEN_INFORMATION_CLASS,
) -> io::Result<Vec<usize>> {
    // SAFETY: the pseudo-handle needs no close; an opened process lives through this call.
    let process = process.map_or_else(
        || unsafe { GetCurrentProcess() },
        AsRawHandle::as_raw_handle,
    );
    let mut token = null_mut();
    // SAFETY: valid process and writable handle output.
    ok(unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) })?;
    let token = owned(token)?;
    let mut size = 0;
    // SAFETY: a null buffer queries the required byte count.
    unsafe { GetTokenInformation(token.as_raw_handle(), class, null_mut(), 0, &raw mut size) };
    let mut buffer = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    // SAFETY: the allocated buffer is at least `size` bytes and aligned.
    ok(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            buffer.as_mut_ptr().cast(),
            size,
            &raw mut size,
        )
    })?;
    Ok(buffer)
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = null_mut();
    // SAFETY: the caller's SID is valid; the output is a LocalAlloc string.
    ok(unsafe { ConvertSidToStringSidW(sid, &raw mut text) })?;
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
    /// The user alone, inherited by nothing: pipes, jobs and single files.
    pub fn user_only() -> io::Result<Self> {
        Self::from_sddl(&format!("D:P(A;;GA;;;{})", user_sid(None)?))
    }

    /// A directory whose every descendant inherits the user's sole access,
    /// whatever its own parent grants. File rights are spelled out (`FA`):
    /// an inherited generic right splits into a mapped and an inherit-only ACE.
    pub fn private_directory() -> io::Result<Self> {
        Self::from_sddl(&format!("D:P(A;OICI;FA;;;{})", user_sid(None)?))
    }

    /// A file the user alone may open, whatever its directory grants.
    pub fn private_file() -> io::Result<Self> {
        Self::from_sddl(&format!("D:P(A;;FA;;;{})", user_sid(None)?))
    }

    pub(crate) fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl = wide(OsStr::new(sddl));
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

    /// The descriptor's DACL, borrowed for as long as `self` lives.
    pub fn dacl(&self) -> io::Result<*const ACL> {
        let (mut present, mut dacl, mut defaulted) = (0, null_mut(), 0);
        // SAFETY: the descriptor is live and every output is writable.
        ok(unsafe {
            GetSecurityDescriptorDacl(self.0, &raw mut present, &raw mut dacl, &raw mut defaulted)
        })?;
        Ok(dacl)
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
