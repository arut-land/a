//! Keeping the machine awake while the daemon works, so a laptop left alone
//! mid-turn does not idle-sleep under it. The display may still sleep.

/// Holds off idle system sleep until dropped. Where the OS offers no way,
/// or refuses, holding it does nothing.
pub struct Awake(#[allow(dead_code)] imp::Hold);

impl Awake {
    /// Takes a hold; `why` is what the OS shows (`pmset -g assertions`,
    /// `systemd-inhibit --list`).
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

#[cfg(target_os = "linux")]
mod imp {
    //! A `systemd-inhibit` lock, held for as long as the process it runs.
    use std::process::{Child, Command, Stdio};

    pub struct Hold(Child, crate::OwnedGroup);

    impl Hold {
        pub fn new(why: &str) -> Option<Self> {
            let mut cmd = Command::new("systemd-inhibit");
            cmd.args(["--what=idle:sleep", "--who=a", "--mode=block"])
                .arg(format!("--why={why}"))
                .args(["sleep", "infinity"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            crate::OwnedGroup::lead(&mut cmd);
            crate::proc::spawn(&mut cmd).ok().map(|child| {
                let group = crate::OwnedGroup::new(&child);
                Self(child, group)
            })
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // Its group: `systemd-inhibit` and the `sleep` it waits on.
            let _ = self.1.signal(crate::Signal::Kill);
            let _ = self.0.wait();
        }
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
mod imp {
    pub struct Hold;

    impl Hold {
        pub fn new(_why: &str) -> Option<Self> {
            None
        }
    }
}

#[cfg(all(test, any(target_vendor = "apple", target_os = "linux")))]
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

    #[test]
    fn holding_never_fails_the_caller() {
        drop(Awake::hold("a test hold"));
    }
}
