//! Private files and directories on Windows. The owner-only policy is part
//! of the create call, so no name is ever reachable under its parent's
//! inherited access, and a private directory's ACE is inheritable, so
//! whatever is created beneath it (by `std` too) is private as well. A child
//! takes its access from its own DACL, never from a parent's traversal check.
//! <https://learn.microsoft.com/en-us/windows/win32/fileio/file-security-and-access-rights>

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetSecurityDescriptorControl, INHERIT_ONLY_ACE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, WRITE_DAC,
};
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

use crate::windows::{Identity, Security, ok, owned, wide};

/// Creates `dir` and its missing parents under the private directory
/// policy, and brings an existing `dir` under it.
pub(super) fn dir(dir: &Path) -> io::Result<()> {
    let dir = std::path::absolute(dir)?;
    let existing = dir
        .ancestors()
        .find(|ancestor| ancestor.symlink_metadata().is_ok())
        .ok_or(io::ErrorKind::NotFound)?;
    let missing = dir
        .strip_prefix(existing)
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    if missing.as_os_str().is_empty() {
        return secure_existing(&dir);
    }
    let security = Security::private_directory()?;
    let attributes = security.attributes();
    let mut path = existing.canonicalize()?;
    let last = missing.components().count() - 1;
    for (index, name) in missing.iter().enumerate() {
        path.push(name);
        let name = wide(path.as_os_str());
        // SAFETY: a NUL-terminated path and attributes whose descriptor lives
        // in `security` through the call.
        match ok(unsafe { CreateDirectoryW(name.as_ptr(), &raw const attributes) }) {
            Ok(()) => {}
            // A concurrent creator; only the final directory's policy matters.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if index == last {
                    return secure_existing(&path);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Creates a new file under the private file policy.
pub(super) fn file(path: &Path, read: bool) -> io::Result<File> {
    let security = Security::private_file()?;
    let attributes = security.attributes();
    let name = wide(extended(path)?.as_os_str());
    // SAFETY: a NUL-terminated path and live attributes; the returned handle
    // is checked and owned. The sharing matches `std`'s default.
    let handle = owned(unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE | if read { GENERIC_READ } else { 0 },
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &raw const attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    })?;
    Ok(File::from(handle))
}

/// An extended-length path whose canonical parent does not follow the
/// final component: a link there is itself the object operated on.
pub(super) fn extended(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent
        .canonicalize()?
        .join(path.file_name().ok_or(io::ErrorKind::InvalidInput)?))
}

/// Refuses a link or another user's directory, and replaces a policy that
/// grants anyone else access. `SetSecurityInfo`, unlike setting the kernel
/// object's security, carries the new inheritable ACE to every descendant
/// that inherits, so what an earlier policy shared is private too.
fn secure_existing(dir: &Path) -> io::Result<()> {
    let handle = OpenOptions::new()
        .access_mode(FILE_GENERIC_READ | WRITE_DAC)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)?;
    let attributes = handle.metadata()?.file_attributes();
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(super::not_private(dir));
    }
    let identity = Identity::current()?;
    let current = Descriptor::of(&handle)?;
    if !identity.owns(current.owner) {
        return Err(super::not_private(dir));
    }
    if current.is_private_directory(identity.user())? {
        return Ok(());
    }
    let policy = Security::private_directory()?;
    // SAFETY: the handle and the policy's DACL remain valid during the call.
    let status = unsafe {
        SetSecurityInfo(
            handle.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            policy.dacl()?,
            null(),
        )
    };
    match status {
        0 => Ok(()),
        error => Err(io::Error::from_raw_os_error(error.cast_signed())),
    }
}

/// An object's owner and DACL, both pointing into one allocation.
struct Descriptor {
    owner: PSID,
    dacl: *mut ACL,
    allocation: PSECURITY_DESCRIPTOR,
}

impl Descriptor {
    fn of(file: &File) -> io::Result<Self> {
        let (mut owner, mut dacl, mut descriptor) = (null_mut(), null_mut(), null_mut());
        // SAFETY: a live handle and writable outputs; the descriptor is
        // LocalAlloc'd and owns what `owner` and `dacl` point to.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &raw mut owner,
                null_mut(),
                &raw mut dacl,
                null_mut(),
                &raw mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status.cast_signed()));
        }
        Ok(Self {
            owner,
            dacl,
            allocation: descriptor,
        })
    }

    /// Exactly the private directory policy: protected from inheritance,
    /// one ACE allowing `user` all file rights, inherited by every child.
    fn is_private_directory(&self, user: PSID) -> io::Result<bool> {
        let (mut control, mut revision) = (0, 0);
        // SAFETY: the descriptor is live and both outputs are writable.
        ok(unsafe {
            GetSecurityDescriptorControl(self.allocation, &raw mut control, &raw mut revision)
        })?;
        if control & SE_DACL_PROTECTED == 0
            || self.dacl.is_null()
            // SAFETY: a non-null DACL points into the live descriptor.
            || unsafe { (*self.dacl).AceCount } != 1
        {
            return Ok(false);
        }
        let mut ace = null_mut();
        // SAFETY: the DACL holds one ACE; the output borrows from it.
        ok(unsafe { GetAce(self.dacl, 0, &raw mut ace) })?;
        // SAFETY: every ACE begins with its header; only an allowed ACE is
        // read as one, and its SID starts at `SidStart` within the ACE.
        Ok(unsafe {
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            let inherit = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
            u32::from(ace.Header.AceType) == ACCESS_ALLOWED_ACE_TYPE
                && u32::from(ace.Header.AceFlags) & (inherit | INHERIT_ONLY_ACE) == inherit
                && ace.Mask == FILE_ALL_ACCESS
                && EqualSid((&raw const ace.SidStart).cast_mut().cast(), user) != 0
        })
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: GetSecurityInfo allocated the descriptor with LocalAlloc.
        unsafe { LocalFree(self.allocation) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    struct Temporary(PathBuf);

    impl Temporary {
        /// A directory shared with Everyone, as a home's parent may be.
        fn shared(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("a-sys-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            let sid = crate::windows::user_sid(None).unwrap();
            let shared =
                Security::from_sddl(&format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;WD)")).unwrap();
            let handle = OpenOptions::new()
                .access_mode(FILE_GENERIC_READ | WRITE_DAC)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(&path)
                .unwrap();
            // SAFETY: the handle and the DACL remain valid during the call.
            let status = unsafe {
                SetSecurityInfo(
                    handle.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    shared.dacl().unwrap(),
                    null(),
                )
            };
            assert_eq!(status, 0);
            Self(path)
        }
    }

    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Every access rule on each path, as `path|sid|type|rights`, read by
    /// PowerShell rather than by the code under test.
    fn rules(paths: &[PathBuf]) -> Vec<String> {
        let script = paths
            .iter()
            .map(|path| {
                let path = path.display().to_string().replace('\'', "''");
                format!(
                    "foreach ($r in (Get-Acl -LiteralPath '{path}').Access) {{ \
                     '{path}|' + $r.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value \
                     + '|' + $r.AccessControlType + '|' + [int]$r.FileSystemRights }}"
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let output = Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!("$ErrorActionPreference = 'Stop'; {script}"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn assert_owners_alone(paths: &[PathBuf]) {
        let sid = crate::windows::user_sid(None).unwrap();
        let rules = rules(paths);
        for path in paths {
            let path = path.display().to_string();
            let own: Vec<_> = rules
                .iter()
                .filter(|rule| rule.starts_with(&format!("{path}|")))
                .collect();
            assert!(!own.is_empty(), "{path} has no rules: {rules:?}");
            for rule in own {
                assert_eq!(
                    rule.strip_prefix(&format!("{path}|")).unwrap(),
                    format!("{sid}|Allow|{FILE_ALL_ACCESS}"),
                    "{rules:?}"
                );
            }
        }
    }

    #[test]
    fn everything_created_in_a_home_under_a_shared_parent_is_the_owners_alone() {
        let parent = Temporary::shared("acl-new");
        let home = parent.0.join("home");
        super::super::private_dir(&home.join("run")).unwrap();
        let created = [
            home.clone(),
            home.join("run"),
            home.join("run").join("start.lock"),
            home.join("config.toml"),
            home.join("settings.toml"),
            home.join("sessions"),
            home.join("sessions").join("log"),
        ];
        fs::write(&created[2], b"").unwrap();
        super::super::private_file().open(&created[3]).unwrap();
        super::super::write_private(&created[4], b"x = 1").unwrap();
        fs::create_dir(&created[5]).unwrap();
        fs::write(&created[6], b"{}").unwrap();
        assert_owners_alone(&created);
    }

    #[test]
    fn a_private_file_ignores_what_its_directory_shares() {
        let parent = Temporary::shared("acl-file");
        let file = parent.0.join("secret");
        super::super::private_file().read(true).open(&file).unwrap();
        assert_owners_alone(std::slice::from_ref(&file));
        assert_eq!(
            super::super::private_file().open(&file).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn an_existing_home_that_shares_is_made_private_with_its_contents() {
        let parent = Temporary::shared("acl-existing");
        let home = parent.0.join("home");
        let contents = [
            home.join("sessions"),
            home.join("sessions").join("log"),
            home.join("logs.jsonl"),
        ];
        fs::create_dir_all(&contents[0]).unwrap();
        fs::write(&contents[1], b"{}").unwrap();
        fs::write(&contents[2], b"{}").unwrap();
        assert!(
            rules(std::slice::from_ref(&contents[1]))
                .iter()
                .any(|rule| rule.contains("|S-1-1-0|"))
        );
        super::super::private_dir(&home).unwrap();
        let mut all = vec![home];
        all.extend(contents);
        assert_owners_alone(&all);
    }

    #[test]
    fn private_dir_never_adopts_a_planted_junction() {
        let parent = Temporary::shared("acl-junction");
        let target = parent.0.join("target");
        fs::create_dir(&target).unwrap();
        let link = parent.0.join("link");
        let status = Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success());
        let before = rules(std::slice::from_ref(&target));
        assert_eq!(
            super::super::private_dir(&link).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(rules(&[target]), before);
    }
}
