//! A home's long-term key: an X25519 secret, made the first time the home
//! needs one and kept where only its user can read it. The home's machine
//! identity is a hash of its public half; nothing exchanges keys with it
//! yet. It is a file in the home's state directory, created mode 0600 on
//! Unix; on Windows it is sealed to the user's logon credentials with
//! DPAPI as well. Why not the macOS keychain: docs/perf/machine-key.md.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use ed25519_compact::x25519;
use sha2::{Digest, Sha256};

use crate::Paths;

/// The home's machine identity: the first 128 bits of the SHA-256 of its
/// public key, the key made and stored first if the home has none. Every
/// process of the home that asks gets the same one. Blocking.
pub fn machine_id(home: &Paths) -> io::Result<[u8; 16]> {
    let path = home.machine_key_file();
    let secret = match fs::read(&path) {
        Ok(sealed) => unseal(&path, &sealed)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => create(&path)?,
        Err(e) => return Err(e),
    };
    let public = x25519::SecretKey::new(secret)
        .recover_public_key()
        .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
    let digest = Sha256::digest(*public);
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    Ok(id)
}

/// Makes the key at `path`, unless another process gets there first: then
/// the home has that one.
fn create(path: &Path) -> io::Result<[u8; 32]> {
    let mut secret = [0; 32];
    getrandom::fill(&mut secret)?;
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(dir)?;
    let mut nonce = [0; 8];
    getrandom::fill(&mut nonce)?;
    let staged = dir.join(format!(".machine.key.{:x}", u64::from_ne_bytes(nonce)));
    let made = (|| {
        let mut file = crate::fs::private_file().open(&staged)?;
        file.write_all(&native::seal(&secret)?)?;
        crate::fs::sync_data(&file)?;
        drop(file);
        crate::fs::publish_ordered(&staged, path)
    })();
    match made {
        Ok(()) => Ok(secret),
        Err(e) => {
            let _ = fs::remove_file(&staged);
            if e.kind() != io::ErrorKind::AlreadyExists {
                return Err(e);
            }
            unseal(path, &fs::read(path)?)
        }
    }
}

fn unseal(path: &Path, sealed: &[u8]) -> io::Result<[u8; 32]> {
    native::unseal(sealed)
        .and_then(|secret| {
            <[u8; 32]>::try_from(secret.as_slice()).map_err(|_| io::ErrorKind::InvalidData.into())
        })
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{} is not this home's machine key: {e}", path.display()),
            )
        })
}

#[cfg(unix)]
mod native {
    use std::io;

    /// The file's mode keeps it the user's.
    #[allow(clippy::unnecessary_wraps, reason = "as Windows seals")]
    pub fn seal(secret: &[u8]) -> io::Result<Vec<u8>> {
        Ok(secret.to_vec())
    }

    #[allow(clippy::unnecessary_wraps, reason = "as Windows unseals")]
    pub fn unseal(sealed: &[u8]) -> io::Result<Vec<u8>> {
        Ok(sealed.to_vec())
    }
}

#[cfg(windows)]
mod native {
    use std::io;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    pub fn seal(secret: &[u8]) -> io::Result<Vec<u8>> {
        let input = blob(secret)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` borrows `secret` for the call; `output` is
        // allocated by the call and freed by `taken`.
        crate::windows::ok(unsafe {
            CryptProtectData(
                &raw const input,
                null(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &raw mut output,
            )
        })?;
        Ok(taken(output))
    }

    pub fn unseal(sealed: &[u8]) -> io::Result<Vec<u8>> {
        let input = blob(sealed)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as `seal`; no description is asked for.
        crate::windows::ok(unsafe {
            CryptUnprotectData(
                &raw const input,
                null_mut(),
                null(),
                null(),
                null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &raw mut output,
            )
        })?;
        Ok(taken(output))
    }

    fn blob(bytes: &[u8]) -> io::Result<CRYPT_INTEGER_BLOB> {
        Ok(CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(bytes.len()).map_err(|_| io::ErrorKind::InvalidInput)?,
            // The API takes a mutable pointer it only reads through.
            pbData: bytes.as_ptr().cast_mut(),
        })
    }

    /// The bytes DPAPI allocated, copied out and freed.
    fn taken(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: a successful call left `cbData` bytes at `pbData`, which
        // `LocalFree` releases once they are copied.
        unsafe {
            let bytes = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
            LocalFree(blob.pbData.cast());
            bytes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_home_keeps_one_machine_id_for_every_process_that_asks() {
        let root = tempfile::tempdir().unwrap();
        let home = Paths::from(root.path().to_owned());
        let racers: Vec<_> = (0..8)
            .map(|_| {
                let home = home.clone();
                std::thread::spawn(move || machine_id(&home).unwrap())
            })
            .collect();
        let ids: Vec<_> = racers.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
        assert_eq!(machine_id(&home).unwrap(), ids[0]);
        // Only the key: no staged copy is left behind.
        let names: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["machine.key"]);
        let other = tempfile::tempdir().unwrap();
        assert_ne!(
            machine_id(&Paths::from(other.path().to_owned())).unwrap(),
            ids[0]
        );
    }

    #[test]
    fn the_key_is_this_users_alone() {
        let root = tempfile::tempdir().unwrap();
        let home = Paths::from(root.path().to_owned());
        machine_id(&home).unwrap();
        let stored = fs::read(home.machine_key_file()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(home.machine_key_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
            assert_eq!(stored.len(), 32);
        }
        // Sealed: the secret is not in the file as it is.
        #[cfg(windows)]
        assert!(stored.len() > 32);
    }

    #[test]
    fn a_damaged_key_is_reported_never_replaced() {
        let root = tempfile::tempdir().unwrap();
        let home = Paths::from(root.path().to_owned());
        fs::write(home.machine_key_file(), b"short").unwrap();
        let error = machine_id(&home).unwrap_err();
        assert!(error.to_string().contains("machine.key"), "{error}");
        assert_eq!(fs::read(home.machine_key_file()).unwrap(), b"short");
    }
}
