//! The console `a` asks for when nothing gave it one. Its Windows manifest
//! asks for no console, so a logon entry opens no window; a person who
//! starts `a` from Explorer or the Run dialog still gets one here.

/// Allocates a console when the process has none and no standard handles,
/// as the OS did before the manifest's policy. Elsewhere, and on Windows
/// releases that ignore the policy, there is nothing to do.
pub fn attach_when_launched_without_one() {
    #[cfg(windows)]
    imp::attach();
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleWindow, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

    /// `ALLOC_CONSOLE_OPTIONS`, absent from windows-sys 0.61.
    #[repr(C)]
    struct Options {
        mode: i32,
        use_show_window: i32,
        show_window: u16,
    }
    /// `ALLOC_CONSOLE_MODE_DEFAULT`: a console the way the OS would have made one.
    const DEFAULT: i32 = 0;
    type Allocate = unsafe extern "system" fn(*const Options, *mut i32) -> i32;

    pub fn attach() {
        // SAFETY: neither call takes arguments that could be invalid.
        if unsafe { !GetConsoleWindow().is_null() } {
            return;
        }
        let wired = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .into_iter()
            // SAFETY: a standard handle identifier.
            .map(|which| unsafe { GetStdHandle(which) })
            .any(|handle| !handle.is_null() && handle != INVALID_HANDLE_VALUE);
        if wired {
            // A parent connected pipes or files: it wants no console either.
            return;
        }
        let kernel: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
        // SAFETY: kernel32 is loaded in every process; the name is terminated.
        let Some(found) = (unsafe {
            GetProcAddress(
                GetModuleHandleW(kernel.as_ptr()),
                c"AllocConsoleWithOptions".as_ptr().cast(),
            )
        }) else {
            return;
        };
        // SAFETY: AllocConsoleWithOptions has this signature where it exists.
        let allocate: Allocate = unsafe { std::mem::transmute(found) };
        let options = Options {
            mode: DEFAULT,
            use_show_window: 0,
            show_window: 0,
        };
        let mut result = 0;
        // SAFETY: both pointers are valid for the call.
        unsafe { allocate(&raw const options, &raw mut result) };
    }
}
