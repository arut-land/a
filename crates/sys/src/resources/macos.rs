use super::Storage;
use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_io_kit::{
    IOBSDNameMatching, IOObjectRelease, IORegistryEntrySearchCFProperty,
    IOServiceGetMatchingService, kIORegistryIterateParents, kIORegistryIterateRecursively,
};
use std::{ffi::CStr, os::unix::ffi::OsStrExt, path::Path};

pub(super) fn storage(path: &Path) -> Storage {
    lookup(path).unwrap_or(Storage::Unknown)
}

fn lookup(path: &Path) -> Option<Storage> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut mount = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is terminated and mount points to writable statfs storage.
    if unsafe { libc::statfs(path.as_ptr(), mount.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statfs succeeded and initialized mount, including its terminated name.
    let mount = unsafe { mount.assume_init() };
    // SAFETY: f_mntfromname is a terminated string supplied by statfs.
    let device = unsafe { CStr::from_ptr(mount.f_mntfromname.as_ptr()) };
    let name = device.to_bytes_with_nul().strip_prefix(b"/dev/")?;
    // SAFETY: name is terminated; port zero selects the default IOKit main port.
    let matching = unsafe { IOBSDNameMatching(0, 0, name.as_ptr().cast()) }?;
    // SAFETY: IOKit consumes the matching dictionary's retained reference.
    let service = unsafe {
        IOServiceGetMatchingService(0, Some(CFRetained::<CFDictionary>::from(&matching)))
    };
    if service == 0 {
        return None;
    }
    let mut plane = [0; 128];
    for (out, byte) in plane.iter_mut().zip(b"IOService") {
        *out = byte.cast_signed();
    }
    let key = CFString::from_static_str("Device Characteristics");
    // APFS exposes logical media; walk parents to the backing device rather than
    // enumerate every mount through CFURL, whose caches inflate daemon idle RSS.
    // SAFETY: service is owned/live, plane/key are terminated/valid, default allocator.
    let properties = unsafe {
        IORegistryEntrySearchCFProperty(
            service,
            &raw mut plane,
            Some(&key),
            None,
            kIORegistryIterateRecursively | kIORegistryIterateParents,
        )
    };
    IOObjectRelease(service);
    let dictionary = properties?.downcast::<CFDictionary>().ok()?;
    // SAFETY: IOKit's property dictionaries use CFString keys and CFType values.
    let dictionary =
        unsafe { CFRetained::cast_unchecked::<CFDictionary<CFString, CFType>>(dictionary) };
    let medium = dictionary
        .get(&CFString::from_static_str("Medium Type"))?
        .downcast::<CFString>()
        .ok()?;
    Some(match medium.to_string().as_str() {
        "Solid State" => Storage::SolidState,
        "Rotational" => Storage::Rotational,
        _ => Storage::Unknown,
    })
}
