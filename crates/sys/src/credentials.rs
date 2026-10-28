//! OAuth credentials are instance-scoped, outside session histories.

use std::io;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// An instance's native secret store. Debug output never contains secrets.
#[derive(Clone, Debug)]
pub struct Credentials {
    namespace: String,
    files: Option<PathBuf>,
}

impl Credentials {
    /// Resolves storage without reading or creating a credential.
    pub fn from_env() -> io::Result<Self> {
        let home = crate::Paths::from_env()?;
        Ok(Self {
            namespace: hex_digest(home.state_dir().as_os_str().as_encoded_bytes()),
            files: if cfg!(target_os = "linux") {
                Some(home.state_dir().join("credentials"))
            } else {
                None
            },
        })
    }

    /// An isolated file store for integration tests; never uses native accounts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn isolated(path: PathBuf) -> Self {
        Self {
            namespace: "a-mcp-test".into(),
            files: Some(path),
        }
    }

    /// Loads one resource's credential, without logging its contents.
    pub fn load(&self, resource: &str) -> io::Result<Option<Vec<u8>>> {
        let key = format!("{}-{}", self.namespace, hex_digest(resource.as_bytes()));
        if let Some(files) = &self.files {
            return match std::fs::read(files.join(key)) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            };
        }
        native::load(&key)
    }

    /// Replaces or removes one resource's credential.
    pub fn save(&self, resource: &str, secret: Option<&[u8]>) -> io::Result<()> {
        let key = format!("{}-{}", self.namespace, hex_digest(resource.as_bytes()));
        if let Some(files) = &self.files {
            let path = files.join(key);
            return if let Some(secret) = secret {
                crate::fs::private_dir(files)?;
                crate::fs::write_private(&path, secret)
            } else {
                match std::fs::remove_file(path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    result => result,
                }
            };
        }
        native::save(&key, secret)
    }
}

/// Independent cryptographic nonces for PKCE and redirect state.
pub fn oauth_nonce() -> io::Result<[u8; 32]> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes)
}

#[cfg(target_os = "linux")]
mod native {
    use std::io;
    pub(super) fn load(_: &str) -> io::Result<Option<Vec<u8>>> {
        unreachable!("Linux uses the instance file store")
    }
    pub(super) fn save(_: &str, _: Option<&[u8]>) -> io::Result<()> {
        unreachable!("Linux uses the instance file store")
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::c_void;
    use std::io;
    use std::ptr;

    type Ref = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDictionaryCreateMutable(
            allocator: Ref,
            capacity: isize,
            keys: Ref,
            values: Ref,
        ) -> Ref;
        fn CFDictionarySetValue(dictionary: Ref, key: Ref, value: Ref);
        fn CFStringCreateWithBytes(
            allocator: Ref,
            bytes: *const u8,
            length: isize,
            encoding: u32,
            external: bool,
        ) -> Ref;
        fn CFDataCreate(allocator: Ref, bytes: *const u8, length: isize) -> Ref;
        fn CFDataGetLength(data: Ref) -> isize;
        fn CFDataGetBytePtr(data: Ref) -> *const u8;
        fn CFRelease(value: Ref);
        static kCFBooleanTrue: Ref;
        static kCFTypeDictionaryKeyCallBacks: [usize; 6];
        static kCFTypeDictionaryValueCallBacks: [usize; 5];
    }
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecItemCopyMatching(query: Ref, result: *mut Ref) -> i32;
        fn SecItemAdd(attributes: Ref, result: *mut Ref) -> i32;
        fn SecItemUpdate(query: Ref, attributes: Ref) -> i32;
        fn SecItemDelete(query: Ref) -> i32;
        static kSecClass: Ref;
        static kSecClassGenericPassword: Ref;
        static kSecAttrService: Ref;
        static kSecAttrAccount: Ref;
        static kSecValueData: Ref;
        static kSecReturnData: Ref;
    }
    struct Owned(Ref);
    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: each Owned holds one create/copy reference.
            unsafe { CFRelease(self.0) }
        }
    }
    fn string(value: &str) -> Owned {
        // SAFETY: bytes live for the call and CF copies them; UTF-8 encoding.
        Owned(unsafe {
            CFStringCreateWithBytes(
                ptr::null(),
                value.as_ptr(),
                value.len().cast_signed(),
                0x0800_0100,
                false,
            )
        })
    }
    fn dictionary() -> Owned {
        // SAFETY: CF's static callbacks retain and compare CF objects in the dictionary.
        Owned(unsafe {
            CFDictionaryCreateMutable(
                ptr::null(),
                0,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            )
        })
    }
    fn query(key: &str) -> Owned {
        let service = string("land.arut.a.mcp");
        let account = string(key);
        let query = dictionary();
        // SAFETY: constants and strings are live, and the dictionary retains its values.
        unsafe {
            CFDictionarySetValue(query.0, kSecClass, kSecClassGenericPassword);
            CFDictionarySetValue(query.0, kSecAttrService, service.0);
            CFDictionarySetValue(query.0, kSecAttrAccount, account.0);
        }
        query
    }
    fn check(status: i32) -> io::Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::other(format!("Keychain status {status}")))
        }
    }
    pub(super) fn load(key: &str) -> io::Result<Option<Vec<u8>>> {
        let query = query(key);
        let mut result = ptr::null();
        // SAFETY: query is live and result receives an owned CFData on success.
        let status = unsafe {
            CFDictionarySetValue(query.0, kSecReturnData, kCFBooleanTrue);
            SecItemCopyMatching(query.0, &raw mut result)
        };
        if status == -25300 {
            return Ok(None);
        }
        check(status)?;
        let result = Owned(result);
        // SAFETY: kSecReturnData requests CFData and its length is nonnegative.
        if unsafe { CFDataGetLength(result.0) } == 0 {
            return Err(io::Error::other("invalid Keychain credential"));
        }
        // SAFETY: nonempty CFData bytes live until result drops.
        let encoded = unsafe {
            std::slice::from_raw_parts(
                CFDataGetBytePtr(result.0),
                CFDataGetLength(result.0).cast_unsigned(),
            )
        };
        Ok(Some(encoded[1..].to_vec()))
    }
    pub(super) fn save(key: &str, secret: Option<&[u8]>) -> io::Result<()> {
        let query = query(key);
        let Some(secret) = secret else {
            // SAFETY: query and all borrowed members remain alive for the call.
            let status = unsafe { SecItemDelete(query.0) };
            return if status == -25300 {
                Ok(())
            } else {
                check(status)
            };
        };
        // The macOS file Keychain treats an empty update as no change.
        // A byte of framing lets every secret, including empty bytes, replace it.
        let secret: Vec<_> = [0].into_iter().chain(secret.iter().copied()).collect();
        // SAFETY: CF copies the bytes. Dictionary values are borrowed until the call ends.
        let data = Owned(unsafe {
            CFDataCreate(ptr::null(), secret.as_ptr(), secret.len().cast_signed())
        });
        let attributes = dictionary();
        // SAFETY: dictionaries and their borrowed keys/values are live throughout.
        let status = unsafe {
            CFDictionarySetValue(attributes.0, kSecValueData, data.0);
            let status = SecItemUpdate(query.0, attributes.0);
            if status == -25300 {
                CFDictionarySetValue(query.0, kSecValueData, data.0);
                SecItemAdd(query.0, ptr::null_mut())
            } else {
                status
            }
        };
        check(status)
    }
}

#[cfg(windows)]
mod native {
    use std::ffi::c_void;
    use std::io;
    use std::ptr;

    #[repr(C)]
    struct Credential {
        flags: u32,
        kind: u32,
        target: *mut u16,
        comment: *mut u16,
        written: [u32; 2],
        length: u32,
        blob: *mut u8,
        persist: u32,
        count: u32,
        attributes: *mut c_void,
        alias: *mut u16,
        user: *mut u16,
    }
    #[link(name = "Advapi32")]
    unsafe extern "system" {
        fn CredReadW(
            target: *const u16,
            kind: u32,
            flags: u32,
            result: *mut *mut Credential,
        ) -> i32;
        fn CredWriteW(credential: *const Credential, flags: u32) -> i32;
        fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
        fn CredFree(value: *const c_void);
    }
    // Credential Manager limits each blob to 2560 bytes. Splitting removes
    // that platform limit from the OAuth token format.
    pub(super) fn load(key: &str) -> io::Result<Option<Vec<u8>>> {
        let Some(count) = read(key)? else {
            return Ok(None);
        };
        let count = u64::from_le_bytes(
            count
                .try_into()
                .map_err(|_| io::Error::other("invalid credential manifest"))?,
        );
        let mut bytes = Vec::new();
        for index in 0..count {
            bytes.extend(
                read(&format!("{key}-{index}"))?
                    .ok_or_else(|| io::Error::other("missing credential part"))?,
            );
        }
        Ok(Some(bytes))
    }
    pub(super) fn save(key: &str, secret: Option<&[u8]>) -> io::Result<()> {
        let old = read(key)?
            .map(|count| {
                count
                    .try_into()
                    .map(u64::from_le_bytes)
                    .map_err(|_| io::Error::other("invalid credential manifest"))
            })
            .transpose()?
            .unwrap_or(0);
        let count = if let Some(secret) = secret {
            for (index, part) in secret.chunks(2400).enumerate() {
                write(&format!("{key}-{index}"), Some(part))?;
            }
            let count = secret.len().div_ceil(2400) as u64;
            write(key, Some(&count.to_le_bytes()))?;
            count
        } else {
            write(key, None)?;
            0
        };
        for index in count..old {
            write(&format!("{key}-{index}"), None)?;
        }
        Ok(())
    }
    fn read(key: &str) -> io::Result<Option<Vec<u8>>> {
        let key: Vec<u16> = key.encode_utf16().chain([0]).collect();
        let mut result = ptr::null_mut();
        // SAFETY: target is NUL terminated; result receives the CredFree-owned allocation.
        if unsafe { CredReadW(key.as_ptr(), 1, 0, &raw mut result) } == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(1168) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: successful CredReadW returns a valid credential and blob until CredFree.
        let bytes = unsafe {
            if (*result).length == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts((*result).blob, (*result).length as usize).to_vec()
            }
        };
        // SAFETY: result is owned by this successful CredReadW.
        unsafe { CredFree(result.cast()) };
        Ok(Some(bytes))
    }
    fn write(key: &str, secret: Option<&[u8]>) -> io::Result<()> {
        let mut key: Vec<u16> = key.encode_utf16().chain([0]).collect();
        let success = if let Some(secret) = secret {
            let credential = Credential {
                flags: 0,
                kind: 1,
                target: key.as_mut_ptr(),
                comment: ptr::null_mut(),
                written: [0; 2],
                length: secret.len().try_into().map_err(io::Error::other)?,
                blob: secret.as_ptr().cast_mut(),
                persist: 2,
                count: 0,
                attributes: ptr::null_mut(),
                alias: ptr::null_mut(),
                user: ptr::null_mut(),
            };
            // SAFETY: Credential layout matches CREDENTIALW; pointers remain live for this call.
            unsafe { CredWriteW(&raw const credential, 0) }
        } else {
            // SAFETY: target is NUL terminated and live through the call.
            let result = unsafe { CredDeleteW(key.as_ptr(), 1, 0) };
            if result == 0 && io::Error::last_os_error().raw_os_error() == Some(1168) {
                return Ok(());
            }
            result
        };
        if success == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            write!(text, "{byte:02x}").expect("write to String");
            text
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support native private-file publication")]
    fn credentials_are_resource_scoped_replaced_and_revoked_outside_logs() {
        let directory = tempfile::tempdir().unwrap();
        let store = Credentials::isolated(directory.path().join("credentials"));
        assert_eq!(store.load("one").unwrap(), None);
        store.save("one", Some(b"secret-one")).unwrap();
        store.save("two", Some(b"secret-two")).unwrap();
        store.save("one", Some(b"replacement")).unwrap();
        assert_eq!(
            store.load("one").unwrap().as_deref(),
            Some(b"replacement".as_slice())
        );
        assert_eq!(
            store.load("two").unwrap().as_deref(),
            Some(b"secret-two".as_slice())
        );
        store.save("one", None).unwrap();
        assert_eq!(store.load("one").unwrap(), None);
        assert!(store.load("two").unwrap().is_some());
    }
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support native credential stores")]
    fn native_credentials_round_trip_tokens_larger_than_a_windows_blob() {
        struct Lease(Credentials);
        impl Drop for Lease {
            fn drop(&mut self) {
                let _ = self.0.save("native-test", None);
            }
        }
        let store = Lease(Credentials {
            namespace: format!("a-test-{}", hex_digest(&oauth_nonce().unwrap())),
            files: None,
        });
        assert!(store.0.load("native-test").unwrap().is_none());
        for bytes in [vec![42; 10_000], Vec::new(), b"replacement".to_vec()] {
            store.0.save("native-test", Some(&bytes)).unwrap();
            assert!(
                store.0.load("native-test").unwrap().as_deref() == Some(bytes.as_slice()),
                "native credential mismatch at length {}",
                bytes.len()
            );
        }
        store.0.save("native-test", None).unwrap();
        assert!(store.0.load("native-test").unwrap().is_none());
    }
}
