//! A private namespace whose children are addressed through its owned handle.

use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::Path;

pub(super) const CONTENT: &str = "content";
pub(super) const ORIGINAL: &str = "original";

#[cfg(windows)]
pub(super) struct DirectoryHandle(Option<File>);

#[cfg(windows)]
impl std::ops::Deref for DirectoryHandle {
    type Target = File;
    fn deref(&self) -> &File {
        self.0.as_ref().expect("directory is pinned until cleanup")
    }
}

pub(super) struct Directory<'a> {
    parent: &'a File,
    #[cfg(unix)]
    pub name: OsString,
    #[cfg(unix)]
    pub file: File,
    #[cfg(windows)]
    pub file: DirectoryHandle,
    retain: bool,
    pub path: std::path::PathBuf,
}

impl<'a> Directory<'a> {
    pub fn create(parent: &'a File, path: &Path) -> io::Result<Self> {
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
            let path = path.join(&name);
            let directory = Self {
                parent,
                #[cfg(unix)]
                name,
                #[cfg(unix)]
                file,
                #[cfg(windows)]
                file: DirectoryHandle(Some(file)),
                retain: false,
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
        if self.retain {
            return;
        }
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
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
                FILE_SHARE_READ, FILE_SHARE_WRITE, ReOpenFile,
            };
            // Keep the object alive after releasing its delete-sharing pin. A native
            // link cannot operate while the directory itself has a DELETE handle.
            // SAFETY: the pinned handle is live; the returned reference is checked/owned.
            let reference = crate::windows::owned(unsafe {
                ReOpenFile(
                    self.file.as_raw_handle(),
                    FILE_READ_ATTRIBUTES,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    FILE_FLAG_BACKUP_SEMANTICS,
                )
            });
            let Ok(reference) = reference.map(File::from) else {
                return;
            };
            drop(self.file.0.take());
            if let Some(name) = self.path.file_name()
                && let Ok(current) = relative(self.parent, name, true, false)
                && super::edit::same_object(&reference, &current).unwrap_or(false)
            {
                delete(&current);
            }
        }
    }
}

pub(super) struct Staged<'a> {
    // Close the child before removing its directory on Windows.
    pub file: File,
    pub directory: Directory<'a>,
    #[cfg(windows)]
    published: bool,
    #[cfg(windows)]
    link_publication: bool,
}

impl<'a> Staged<'a> {
    #[cfg(unix)]
    pub fn create(directory: Directory<'a>) -> io::Result<Self> {
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
        Ok(Self { file, directory })
    }

    #[cfg(windows)]
    pub fn copied(
        directory: Directory<'a>,
        link_publication: bool,
        copy: impl FnOnce(&Directory<'_>) -> io::Result<File>,
    ) -> io::Result<Self> {
        let file = copy(&directory)?;
        Ok(Self {
            file,
            directory,
            published: false,
            link_publication,
        })
    }

    #[cfg(unix)]
    pub fn exchange(&mut self, name: &OsStr) -> io::Result<bool> {
        use rustix::fs::{RenameFlags, renameat_with};
        match renameat_with(
            &self.directory.file,
            CONTENT,
            self.directory.parent,
            name,
            RenameFlags::EXCHANGE,
        ) {
            Ok(()) => Ok(true),
            Err(
                rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::NOTSUP,
            ) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(unix)]
    pub fn displaced(&self) -> io::Result<File> {
        use rustix::fs::{Mode, OFlags, openat};
        Ok(File::from(openat(
            &self.directory.file,
            CONTENT,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?))
    }

    #[cfg(unix)]
    pub fn retain(&mut self) -> std::path::PathBuf {
        self.directory.retain = true;
        self.directory.path.join(CONTENT)
    }

    pub fn take_original(&mut self, source: &File, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            let _ = source;
            rustix::fs::renameat(self.directory.parent, name, &self.directory.file, ORIGINAL)?;
            self.directory.retain = true;
        }
        #[cfg(windows)]
        {
            let _ = name;
            // Hard links preserve the ACL verbatim; cross-directory rename can
            // rewrite inherited ACEs. The caller closes this DELETE handle to
            // complete POSIX unlink before attempting no-replace publication.
            link(source, &self.directory.file, OsStr::new(ORIGINAL))?;
            self.directory.retain = true;
            remove(source)?;
        }
        Ok(())
    }

    pub fn has_original(&self) -> bool {
        self.directory.retain
    }

    pub fn original(&self) -> io::Result<File> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, openat};
            Ok(File::from(openat(
                &self.directory.file,
                ORIGINAL,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?))
        }
        #[cfg(windows)]
        relative(&self.directory.file, OsStr::new(ORIGINAL), false, false)
    }

    pub fn restore_original(&mut self, source: &File, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            let _ = source;
            no_replace(
                &self.directory.file,
                OsStr::new(ORIGINAL),
                self.directory.parent,
                name,
            )?;
            remove_if_present(&self.directory.file, ORIGINAL)?;
        }
        #[cfg(windows)]
        {
            let _ = source;
            let original = self.original()?;
            link(&original, self.directory.parent, name)?;
            self.remove_original_link()?;
        }
        self.directory.retain = false;
        Ok(())
    }

    #[cfg(unix)]
    pub fn prepare_vacancy(&mut self) -> io::Result<()> {
        // Prove that the filesystem has a no-replace path before detaching a target.
        // Both names are exclusively owned; this cannot expose staged content.
        no_replace(
            &self.directory.file,
            OsStr::new(CONTENT),
            &self.directory.file,
            OsStr::new("probe"),
        )?;
        rustix::fs::renameat(&self.directory.file, "probe", &self.directory.file, CONTENT)?;
        remove_if_present(&self.directory.file, "probe")
    }

    #[cfg(windows)]
    pub fn prepare_vacancy(&mut self) -> io::Result<()> {
        // Refuse unsupported filesystems while the target is still untouched.
        link(&self.file, &self.directory.file, OsStr::new("probe"))?;
        self.remove_link("probe")?;
        if super::edit::accessible_links(&self.file)? != 1 {
            return Err(io::ErrorKind::Unsupported.into());
        }
        Ok(())
    }

    #[cfg(windows)]
    fn remove_link(&self, name: &str) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{Foundation::GENERIC_READ, Storage::FileSystem::DELETE};
        let file = relative(&self.directory.file, OsStr::new(name), false, false)?;
        let authority = super::edit::reopen(file.as_raw_handle(), GENERIC_READ | DELETE)?;
        remove(&authority)?;
        drop(authority);
        drop(file);
        Ok(())
    }

    #[cfg(windows)]
    fn remove_original_link(&self) -> io::Result<()> {
        self.remove_link(ORIGINAL)
    }

    pub fn reclaim_original(&mut self, source: &File) -> io::Result<()> {
        #[cfg(unix)]
        {
            let _ = source;
            rustix::fs::unlinkat(&self.directory.file, ORIGINAL, rustix::fs::AtFlags::empty())?;
        }
        #[cfg(windows)]
        {
            let _ = source;
            self.remove_original_link()?;
        }
        self.directory.retain = false;
        Ok(())
    }

    pub fn retain_original(&mut self) -> std::path::PathBuf {
        self.directory.retain = true;
        self.directory.path.join(ORIGINAL)
    }

    pub fn publish(&mut self, name: &OsStr) -> io::Result<()> {
        #[cfg(unix)]
        {
            no_replace(
                &self.directory.file,
                OsStr::new(CONTENT),
                self.directory.parent,
                name,
            )
        }
        #[cfg(windows)]
        {
            if self.link_publication {
                link(&self.file, self.directory.parent, name)?;
            } else {
                rename(&self.file, self.directory.parent, name)?;
                self.published = true;
            }
            Ok(())
        }
    }

    #[cfg(all(unix, test))]
    pub fn fallback(&mut self, name: &OsStr) -> io::Result<()> {
        let source = &self.directory.file;
        let target = self.directory.parent;
        rustix::fs::linkat(source, CONTENT, target, name, rustix::fs::AtFlags::empty())?;
        Ok(())
    }
}

#[cfg(unix)]
fn no_replace(source: &File, from: &OsStr, target: &File, to: &OsStr) -> io::Result<()> {
    use rustix::fs::{AtFlags, RenameFlags, linkat, renameat_with};
    match renameat_with(source, from, target, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::NOTSUP) => {
            // Publication is complete once linkat succeeds. Cleanup is separate:
            // a cleanup error must not turn visible publication into an IO error.
            linkat(source, from, target, to, AtFlags::empty())?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn remove_if_present(directory: &File, name: &str) -> io::Result<()> {
    match rustix::fs::unlinkat(directory, name, rustix::fs::AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        if !self.directory.retain {
            let _ =
                rustix::fs::unlinkat(&self.directory.file, CONTENT, rustix::fs::AtFlags::empty());
        }
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
            GENERIC_WRITE
                | READ_CONTROL
                | WRITE_DAC
                | WRITE_OWNER
                | if directory { 0 } else { DELETE }
        } else if directory {
            DELETE
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
            FILE_SHARE_READ
                | if create { FILE_SHARE_WRITE } else { 0 }
                | if directory { 0 } else { FILE_SHARE_DELETE },
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
    let _ = remove(file);
}

#[cfg(windows)]
fn remove(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO_EX, FileDispositionInfoEx, SetFileInformationByHandle,
    };
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: 0x1 | 0x2 | 0x10,
    }; // DELETE | POSIX_SEMANTICS | IGNORE_READONLY_ATTRIBUTE
    // SAFETY: deletion is bound to the owned object, never a mutable pathname.
    crate::windows::ok(unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const info).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).expect("fixed structure size"),
        )
    })
}

#[cfg(windows)]
fn rename(file: &File, parent: &File, to: &OsStr) -> io::Result<()> {
    name_operation(
        file,
        parent,
        to,
        windows_sys::Wdk::Storage::FileSystem::FileRenameInformationEx,
    )
}

#[cfg(windows)]
fn link(file: &File, parent: &File, to: &OsStr) -> io::Result<()> {
    name_operation(
        file,
        parent,
        to,
        windows_sys::Wdk::Storage::FileSystem::FileLinkInformationEx,
    )
}

#[cfg(windows)]
fn name_operation(
    file: &File,
    parent: &File,
    to: &OsStr,
    class: windows_sys::Wdk::Storage::FileSystem::FILE_INFORMATION_CLASS,
) -> io::Result<()> {
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;
    let name: Vec<u16> = to.encode_wide().collect();
    let bytes = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + name.len() * 2;
    let length = u32::try_from(bytes.max(size_of::<FILE_RENAME_INFO>()))
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut buffer = vec![0u64; bytes.max(size_of::<FILE_RENAME_INFO>()).div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // Extended native link/rename information has the same documented header.
    // SAFETY: aligned storage contains the complete header and UTF-16 tail;
    // both owned handles and the buffer live through the synchronous call.
    unsafe {
        (*info).Anonymous.Flags =
            if class == windows_sys::Wdk::Storage::FileSystem::FileRenameInformationEx {
                0x2
            } else {
                0
            }; // Never REPLACE_IF_EXISTS.
        (*info).RootDirectory = parent.as_raw_handle();
        (*info).FileNameLength = u32::try_from(name.len() * 2).expect("checked buffer length");
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        let mut status = windows_sys::Win32::System::IO::IO_STATUS_BLOCK::default();
        let result = windows_sys::Wdk::Storage::FileSystem::NtSetInformationFile(
            file.as_raw_handle(),
            &raw mut status,
            info.cast(),
            length,
            class,
        );
        if result < 0 {
            let code = windows_sys::Win32::Foundation::RtlNtStatusToDosError(result).cast_signed();
            if matches!(code, 1 | 50 | 87) {
                return Err(io::ErrorKind::Unsupported.into());
            }
            Err(io::Error::from_raw_os_error(code))
        } else {
            Ok(())
        }
    }
}
