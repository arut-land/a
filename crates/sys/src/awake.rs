//! Keeping the machine awake while the daemon works, so a laptop left alone
//! mid-turn does not idle-sleep under it. The display may still sleep.

/// Holds off idle system sleep until dropped. Where the OS offers no way,
/// or refuses, holding it does nothing.
pub struct Awake(#[allow(dead_code)] imp::Hold);

impl Awake {
    /// Takes a hold; `why` is what the OS shows (`pmset -g assertions`,
    /// `powercfg /requests`).
    pub fn hold(why: &str) -> Option<Self> {
        imp::Hold::new(why).map(Self)
    }
}

#[cfg(target_vendor = "apple")]
mod imp {
    //! An `IOKit` power assertion.
    use std::ffi::{CString, c_char, c_void};

    type CFStringRef = *const c_void;

    const UTF8: u32 = 0x0800_0100;
    const ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            text: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(object: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    pub struct Hold(u32);

    /// A CoreFoundation string, released when dropped.
    struct CfString(CFStringRef);

    impl CfString {
        fn new(text: &str) -> Option<Self> {
            let text = CString::new(text).ok()?;
            // SAFETY: `text` is a valid NUL-terminated string for the call;
            // a null allocator means the default one.
            let s = unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) };
            (!s.is_null()).then_some(Self(s))
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            // SAFETY: we own the one reference `CFStringCreateWithCString`
            // returned.
            unsafe { CFRelease(self.0) }
        }
    }

    impl Hold {
        pub fn new(why: &str) -> Option<Self> {
            let kind = CfString::new("PreventUserIdleSystemSleep")?;
            let name = CfString::new(why)?;
            let mut id = 0;
            // SAFETY: both strings live across the call, which copies what
            // it keeps; `id` is a valid place for the result.
            let status = unsafe { IOPMAssertionCreateWithName(kind.0, ON, name.0, &raw mut id) };
            (status == 0).then_some(Self(id))
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: `self.0` is an assertion we created and release once.
            unsafe { IOPMAssertionRelease(self.0) };
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::{AsRawHandle, OwnedHandle};
    use windows_sys::Win32::System::{
        Power::{
            PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
        },
        SystemServices::POWER_REQUEST_CONTEXT_VERSION,
        Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0},
    };

    pub struct Hold(pub(super) OwnedHandle);

    impl Hold {
        pub fn new(why: &str) -> Option<Self> {
            if why.contains('\0') {
                return None;
            }
            let mut reason: Vec<u16> = why.encode_utf16().chain([0]).collect();
            let context = REASON_CONTEXT {
                Version: POWER_REQUEST_CONTEXT_VERSION,
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 {
                    SimpleReasonString: reason.as_mut_ptr(),
                },
            };
            // SAFETY: the context and its NUL-terminated reason live across
            // the call, which copies the reason. The handle is not inherited.
            let handle = crate::windows::owned(unsafe { PowerCreateRequest(&context) }).ok()?;
            // SAFETY: we own a live power request. Failure drops the handle
            // without publishing a hold; success is paired with one clear.
            if unsafe { PowerSetRequest(handle.as_raw_handle(), PowerRequestSystemRequired) } == 0 {
                return None;
            }
            Some(Self(handle))
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: this handle has exactly one system-required request.
            // OwnedHandle closes it even if clearing fails; process death also
            // closes the handle, so an assertion cannot outlive its holder.
            unsafe { PowerClearRequest(self.0.as_raw_handle(), PowerRequestSystemRequired) };
        }
    }
}

#[cfg(not(any(target_vendor = "apple", windows)))]
mod imp {
    // Linux has no existing bus integration. An optional sleep hold does not
    // justify a D-Bus stack, and a helper could outlive a crashed daemon.
    pub struct Hold;

    impl Hold {
        pub fn new(_why: &str) -> Option<Self> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_vendor = "apple")]
    #[test]
    fn the_os_lists_the_hold_while_it_is_held() {
        let why = format!("a test hold {}", std::process::id());
        let listed = || {
            let out = std::process::Command::new("pmset")
                .args(["-g", "assertions"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).contains(&why)
        };
        let hold = Awake::hold(&why).expect("an assertion");
        assert!(listed());
        drop(hold);
        assert!(!listed());
    }

    #[cfg(windows)]
    #[test]
    fn requests_are_owned_and_not_inherited() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};

        let first = Awake::hold("a test hold \u{1f680}").expect("a power request");
        let second = Awake::hold("another test hold").expect("an independent power request");
        let mut flags = 0;
        // SAFETY: the hold owns a live handle and flags is writable.
        assert_ne!(
            unsafe { GetHandleInformation(first.0.0.as_raw_handle(), &raw mut flags) },
            0
        );
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
        drop(first);
        // SAFETY: dropping another hold cannot close this one's handle.
        assert_ne!(
            unsafe { GetHandleInformation(second.0.0.as_raw_handle(), &raw mut flags) },
            0
        );
    }

    #[cfg(any(windows, target_vendor = "apple"))]
    #[test]
    fn a_reason_with_nul_is_rejected() {
        assert!(Awake::hold("a\0hidden reason").is_none());
    }

    #[cfg(not(any(windows, target_vendor = "apple")))]
    #[test]
    fn unsupported_platforms_allocate_no_hold() {
        assert!(Awake::hold("a test hold").is_none());
    }

    #[test]
    fn holding_never_fails_the_caller() {
        drop(Awake::hold("a test hold"));
    }
}
