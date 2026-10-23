//! A private namespace whose children are addressed through its owned handle.

use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::Path;

pub(super) const CONTENT: &str = "content";

pub(super) struct Directory<'a> {
    parent: &'a File,
    #[cfg(unix)]
    pub name: OsString,
    pub file: File,
    #[cfg(windows)]
    pub path: std::path::PathBuf,
}

impl<'a> Directory<'a> {
    pub fn create(parent: &'a File, path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        let _ = path;
        loop {
            let name = super::edit::temporary_name();
            #[cfg(unix)]
            let created = {
                use rustix::fs::{Mode, OFlags, mkdirat, openat};
                mkdirat(parent, &name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
                    .map_err(io::Error::from)
                    .and_then(|()| {
                        openat(
                            parent,
                            &name,
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map(File::from)
                        .map_err(io::Error::from)
                    })
            };
            #[cfg(windows)]
            let created = relative(parent, &name, true, true);
            let file = match created {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // mkdir/open is not one syscall. Refuse a replacement owned by
                // another directory writer before trusting this namespace.
                // SAFETY: geteuid has no pointer arguments.
                if file.metadata()?.uid() != unsafe { libc::geteuid() } {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
            }
            #[cfg(windows)]
            let path = path.join(&name);
            let directory = Self {
                parent,
                #[cfg(unix)]
                name,
                file,
                #[cfg(windows)]
                path,
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                #[cfg(target_vendor = "apple")]
                super::edit::access_acl(None, &directory.file)?;
                directory
                    .file
                    .set_permissions(std::fs::Permissions::from_mode(0o700))?;
            }
            return Ok(directory);
        }
    }
}

impl Drop for Directory<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, fstat, statat, unlinkat};
            if let (Ok(ours), Ok(named)) = (
                fstat(&self.file),
                statat(self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW),
            ) && (ours.st_dev, ours.st_ino) == (named.st_dev, named.st_ino)
            {
                // Never recurse or follow a substituted parent entry.
                let _ = unlinkat(self.parent, &self.name, AtFlags::REMOVEDIR);
            }
        }
        #[cfg(windows)]
        delete(&self.file);
    }
}

pub(super) struct Staged<'a> {
    // Close the child before removing its directory on Windows.
    pub file: File,
    pub directory: Directory<'a>,
    #[cfg(target_os = "linux")]
    anonymous: bool,
    #[cfg(windows)]
    published: bool,
}

impl<'a> Staged<'a> {
    #[cfg(unix)]
    pub fn create(directory: Directory<'a>) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let anonymous = {
            use rustix::fs::{Mode, OFlags, openat};
            openat(
                &directory.file,
                ".",
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::TMPFILE,
                Mode::RUSR | Mode::WUSR,
            )
        };
        #[cfg(target_os = "linux")]
        match anonymous {
            Ok(file) => {
                return Ok(Self {
                    file: File::from(file),
                    directory,
                    anonymous: true,
                });
            }
            Err(
                rustix::io::Errno::NOTSUP
                | rustix::io::Errno::INVAL
                | rustix::io::Errno::ISDIR
                | rustix::io::Errno::NOSYS,
            ) => {}
            Err(error) => return Err(error.into()),
        }
        Self::named(directory)
    }

    #[cfg(unix)]
    pub fn named(directory: Directory<'a>) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags, openat};
        let flags = OFlags::RDWR | OFlags::CLOEXEC;
        let mode = Mode::RUSR | Mode::WUSR;
        let file = File::from(openat(
            &directory.file,
            CONTENT,
            flags | OFlags::CREATE | OFlags::EXCL,
            mode,
        )?);
        #[cfg(target_vendor = "apple")]
        super::edit::access_acl(None, &file)?;
        Ok(Self {
            file,
            directory,
            #[cfg(target_os = "linux")]
            anonymous: false,
        })
    }

    #[cfg(windows)]
    pub fn copied(
        directory: Directory<'a>,
        copy: impl FnOnce(&Directory<'_>) -> io::Result<File>,
    ) -> io::Result<Self> {
        let file = copy(&directory)?;
        Ok(Self {
            file,
            directory,
            published: false,
        })
    }

    pub fn publish(&mut self, name: &OsStr, replacing: bool) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        self.link()?;
        #[cfg(unix)]
        {
            use rustix::fs::{RenameFlags, renameat_with};
            let flags = if replacing {
                RenameFlags::empty()
            } else {
                RenameFlags::NOREPLACE
            };
            match renameat_with(
                &self.directory.file,
                CONTENT,
                self.directory.parent,
                name,
                flags,
            ) {
                Ok(()) => Ok(()),
                Err(
                    rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::NOTSUP,
                ) => self.fallback(name, replacing),
                Err(error) => Err(error.into()),
            }
        }
        #[cfg(windows)]
        {
            rename(&self.file, self.directory.parent, name, replacing)?;
            self.published = true;
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    fn link(&mut self) -> io::Result<()> {
        if self.anonymous {
            use rustix::fs::{AtFlags, CWD, linkat};
            use std::os::fd::AsRawFd;
            match linkat(
                &self.file,
                "",
                &self.directory.file,
                CONTENT,
                AtFlags::EMPTY_PATH,
            ) {
                Ok(()) => {}
                Err(rustix::io::Errno::PERM | rustix::io::Errno::NOENT) => {
                    // Unprivileged AT_EMPTY_PATH needs the procfs fd alias on
                    // kernels that require CAP_DAC_READ_SEARCH for that flag.
                    let path = format!("/proc/self/fd/{}", self.file.as_raw_fd());
                    linkat(
                        CWD,
                        path,
                        &self.directory.file,
                        CONTENT,
                        AtFlags::SYMLINK_FOLLOW,
                    )?;
                }
                Err(error) => return Err(error.into()),
            }
            self.anonymous = false;
        }
        Ok(())
    }

    #[cfg(unix)]
    pub fn fallback(&mut self, name: &OsStr, replacing: bool) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        self.link()?;
        let source = &self.directory.file;
        let target = self.directory.parent;
        if replacing {
            rustix::fs::renameat(source, CONTENT, target, name)?;
        } else {
            rustix::fs::linkat(source, CONTENT, target, name, rustix::fs::AtFlags::empty())?;
        }
        Ok(())
    }
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = rustix::fs::unlinkat(&self.directory.file, CONTENT, rustix::fs::AtFlags::empty());
        #[cfg(windows)]
        if !self.published {
            delete(&self.file);
        }
    }
}

#[cfg(windows)]
pub(super) fn relative(
    parent: &File,
    name: &OsStr,
    directory: bool,
    create: bool,
) -> io::Result<File> {
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use windows_sys::Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
            FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    };
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, RtlNtStatusToDosError, UNICODE_STRING},
        Storage::FileSystem::{
            DELETE, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            READ_CONTROL, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER,
        },
        System::IO::IO_STATUS_BLOCK,
    };
    let encoded: Vec<u16> = name.encode_wide().collect();
    let length = u16::try_from(encoded.len() * 2).map_err(|_| io::ErrorKind::InvalidInput)?;
    let text = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: encoded.as_ptr().cast_mut(),
    };
    let security = create
        .then(crate::windows::Security::user_only)
        .transpose()?;
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).expect("fixed structure size"),
        Attributes: windows_sys::Win32::Foundation::OBJ_CASE_INSENSITIVE,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &raw const text,
        SecurityDescriptor: security
            .as_ref()
            .map_or(std::ptr::null(), |s| s.descriptor().cast()),
        ..Default::default()
    };
    let mut handle = std::ptr::null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    let access = GENERIC_READ
        | SYNCHRONIZE
        | if create {
            GENERIC_WRITE | DELETE | READ_CONTROL | WRITE_DAC | WRITE_OWNER
        } else {
            0
        };
    // SAFETY: names, descriptor and output structures remain live until the
    // synchronous operation completes; the returned handle is checked/owned.
    let result = unsafe {
        NtCreateFile(
            &raw mut handle,
            access,
            &raw const attributes,
            &raw mut status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | if directory { 0 } else { FILE_SHARE_DELETE },
            if create { FILE_CREATE } else { FILE_OPEN },
            FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_REPARSE_POINT
                | if directory {
                    FILE_DIRECTORY_FILE
                } else if create {
                    FILE_NON_DIRECTORY_FILE
                } else {
                    0
                },
            std::ptr::null(),
            0,
        )
    };
    if result < 0 {
        // SAFETY: converts the scalar NTSTATUS, without dereferencing pointers.
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(result) }.cast_signed(),
        ));
    }
    Ok(File::from(crate::windows::owned(handle)?))
}

#[cfg(windows)]
pub(super) fn delete(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx, SetFileInformationByHandle,
    };
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: 0x1 | 0x2 | 0x10,
    }; // DELETE | POSIX_SEMANTICS | IGNORE_READONLY_ATTRIBUTE
    // SAFETY: deletion is bound to the owned object, never a mutable pathname.
    unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).expect("fixed structure size"),
        );
    }
}

#[cfg(windows)]
fn rename(file: &File, parent: &File, to: &OsStr, replacing: bool) -> io::Result<()> {
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;
    let name: Vec<u16> = to.encode_wide().collect();
    let bytes = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + name.len() * 2;
    let length = u32::try_from(bytes).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut buffer = vec![0u64; bytes.max(size_of::<FILE_RENAME_INFO>()).div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: aligned storage contains the complete header and UTF-16 tail;
    // both owned handles and the buffer live through the synchronous call.
    unsafe {
        (*info).Anonymous.Flags = if replacing { 0x1 | 0x2 } else { 0x2 };
        (*info).RootDirectory = parent.as_raw_handle();
        (*info).FileNameLength = u32::try_from(name.len() * 2).expect("checked buffer length");
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        let mut status = windows_sys::Win32::System::IO::IO_STATUS_BLOCK::default();
        let result = windows_sys::Wdk::Storage::FileSystem::NtSetInformationFile(
            file.as_raw_handle(),
            &raw mut status,
            info.cast(),
            length,
            windows_sys::Wdk::Storage::FileSystem::FileRenameInformationEx,
        );
        if result < 0 {
            Err(io::Error::from_raw_os_error(
                windows_sys::Win32::Foundation::RtlNtStatusToDosError(result).cast_signed(),
            ))
        } else {
            Ok(())
        }
    }
}
