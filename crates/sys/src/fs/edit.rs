//! Safe-save publication. Final-component links and multiply linked files are
//! rejected: replacing either would silently change which object is edited.
//! Revalidation detects observed concurrent changes, but no supported OS offers
//! a pathname/content compare-and-swap against uncooperative writers.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditConflict {
    Changed,
    Appeared,
}

#[derive(Debug)]
pub enum EditError {
    Conflict(EditConflict),
    NotRegular,
    MultipleLinks,
    TooLarge,
    Io(io::Error),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(reason) => {
                write!(f, "edit conflict: {reason:?}; inspect the file and retry")
            }
            Self::NotRegular => {
                f.write_str("edit requires a regular file; symbolic links are rejected")
            }
            Self::MultipleLinks => f.write_str("edit refuses to break multiple hard links"),
            Self::TooLarge => f.write_str("file exceeds byte limit"),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for EditError {}

impl From<io::Error> for EditError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Namespace visibility and power-loss persistence are separate promises.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    NotRequested,
    Confirmed,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditReceipt {
    pub visible: bool,
    pub durability: Durability,
}

/// Retains the original handle and a pinned parent until publication or abort.
pub struct EditTransaction {
    parent: File,
    #[cfg(windows)]
    directory: PathBuf,
    name: OsString,
    source: Option<(File, Signature)>,
    original: Option<Vec<u8>>,
}

impl EditTransaction {
    pub fn capture(path: &Path, limit: usize) -> Result<Self, EditError> {
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
            .to_owned();
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            if name.encode_wide().any(|unit| unit == u16::from(b':')) {
                return Err(EditError::NotRegular);
            }
        }
        let directory = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(directory)?;
        let directory = directory.canonicalize()?;
        let parent = open_parent(&directory)?;
        let mut transaction = Self {
            parent,
            #[cfg(windows)]
            directory,
            name,
            source: None,
            original: None,
        };
        let mut file = match transaction.open_source() {
            Ok(file) => file,
            Err(EditError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(transaction);
            }
            Err(error) => return Err(error),
        };
        let signature = signature(&file)?;
        let mut bytes = Vec::new();
        (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(EditError::TooLarge);
        }
        if signature != self::signature(&file)? {
            return Err(EditError::Conflict(EditConflict::Changed));
        }
        transaction.original = Some(bytes);
        transaction.source = Some((file, signature));
        Ok(transaction)
    }

    pub fn original(&self) -> Option<&[u8]> {
        self.original.as_deref()
    }

    fn open_source(&self) -> Result<File, EditError> {
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, FileType, statat};
            let metadata = statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile {
                return Err(EditError::NotRegular);
            }
        }
        #[cfg(windows)]
        {
            let metadata = fs::symlink_metadata(self.directory.join(&self.name))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(EditError::NotRegular);
            }
        }
        #[cfg(unix)]
        let file = {
            use rustix::fs::{Mode, OFlags, openat};
            File::from(
                openat(
                    &self.parent,
                    &self.name,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| {
                    if error == rustix::io::Errno::LOOP {
                        EditError::NotRegular
                    } else {
                        EditError::Io(error.into())
                    }
                })?,
            )
        };
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(0x0020_0000)
                .open(self.directory.join(&self.name))?
        };
        validate(&file)?;
        Ok(file)
    }

    fn revalidate(&self) -> Result<(), EditError> {
        match (&self.source, self.open_source()) {
            (None, Err(EditError::Io(error))) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            (None, _) => Err(EditError::Conflict(EditConflict::Appeared)),
            (Some((source, before)), Ok(mut current)) => {
                if *before != signature(&current)? || *before != signature(source)? {
                    return Err(EditError::Conflict(EditConflict::Changed));
                }
                let original = self.original.as_deref().expect("source has captured bytes");
                let mut at = 0;
                let mut buffer = [0; 8192];
                loop {
                    let count = current.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    if original.get(at..at + count) != Some(&buffer[..count]) {
                        return Err(EditError::Conflict(EditConflict::Changed));
                    }
                    at += count;
                }
                if at != original.len() || *before != signature(&current)? {
                    return Err(EditError::Conflict(EditConflict::Changed));
                }
                Ok(())
            }
            (Some(_), Err(_)) => Err(EditError::Conflict(EditConflict::Changed)),
        }
    }

    /// Consuming the transaction ensures an abandoned staging file is reclaimed.
    /// A post-rename flush failure is a visible receipt, never an ambiguous error.
    /// Windows handle rename promises visibility only; durable mode is rejected
    /// before writing because there is no documented directory flush barrier.
    pub fn publish(self, bytes: &[u8], durable: bool) -> Result<EditReceipt, EditError> {
        self.publish_with_sync(bytes, durable, File::sync_all)
    }

    fn publish_with_sync(
        self,
        bytes: &[u8],
        durable: bool,
        sync_parent: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<EditReceipt, EditError> {
        #[cfg(windows)]
        if durable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "edit publication supports atomic visibility only on Windows",
            )
            .into());
        }
        let (name, file) = self.stage()?;
        let pending = Pending {
            transaction: &self,
            name,
        };
        let mut file = file;
        validate(&file)?;
        file.rewind()?;
        file.write_all(bytes)?;
        file.set_len(bytes.len() as u64)?;
        if let Some((source, _)) = &self.source {
            preserve_metadata(source, &file)?;
        }
        if durable {
            file.sync_all()?;
        }
        self.revalidate()?;
        match rename(&self, &pending.name, &file, self.source.is_some()) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(EditError::Conflict(EditConflict::Appeared));
            }
            result => result?,
        }
        let durability = if durable {
            match sync_parent(&self.parent) {
                Ok(()) => Durability::Confirmed,
                Err(error) => Durability::Failed(error.to_string()),
            }
        } else {
            Durability::NotRequested
        };
        Ok(EditReceipt {
            visible: true,
            durability,
        })
    }

    fn stage(&self) -> io::Result<(OsString, File)> {
        loop {
            let name = temporary_name();
            #[cfg(unix)]
            let result = {
                use rustix::fs::{Mode, OFlags, openat};
                openat(
                    &self.parent,
                    &name,
                    OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )
                .map(File::from)
                .map_err(io::Error::from)
            };
            #[cfg(windows)]
            let result = stage_windows(self, &name);
            match result {
                Ok(file) => return Ok((name, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }
}

pub(super) fn temporary_name() -> OsString {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        ".a-edit-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
    .into()
}

struct Pending<'a> {
    transaction: &'a EditTransaction,
    name: OsString,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = rustix::fs::unlinkat(
            &self.transaction.parent,
            &self.name,
            rustix::fs::AtFlags::empty(),
        );
        #[cfg(windows)]
        remove_pending(&self.transaction.directory.join(&self.name));
    }
}

fn open_parent(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        Ok(File::from(rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        // Denying delete sharing pins the directory spelling during publication.
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
    }
}

fn validate(file: &File) -> Result<(), EditError> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(EditError::NotRegular);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(EditError::MultipleLinks);
        }
    }
    #[cfg(target_vendor = "apple")]
    {
        use std::os::macos::fs::MetadataExt;
        if metadata.st_flags()
            & (libc::UF_IMMUTABLE | libc::SF_IMMUTABLE | libc::UF_APPEND | libc::SF_APPEND)
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "edit target is immutable or append-only",
            )
            .into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(EditError::NotRegular);
        }
        if windows_information(file)?.nNumberOfLinks != 1 {
            return Err(EditError::MultipleLinks);
        }
    }
    Ok(())
}

#[cfg(unix)]
type Signature = (u64, u64, u64, i64, i64, i64, i64, u32, u32, u32, u64);

#[cfg(unix)]
fn signature(file: &File) -> io::Result<Signature> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok((
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
        m.mode(),
        m.uid(),
        m.gid(),
        m.nlink(),
    ))
}

#[cfg(unix)]
fn preserve_metadata(source: &File, destination: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let metadata = source.metadata()?;
    let current = destination.metadata()?;
    if (metadata.uid(), metadata.gid()) != (current.uid(), current.gid()) {
        // Failure aborts rather than silently changing the user's ownership.
        // SAFETY: destination owns the descriptor; uid/gid came from metadata.
        if unsafe { libc::fchown(destination.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_vendor = "apple")]
    {
        use std::os::macos::fs::MetadataExt;
        let modified = destination.metadata()?.modified()?;
        // Copy birth time, ACLs and xattrs through handles, including resource
        // forks and quarantine. The content edit gets its own modification time.
        // SAFETY: both descriptors remain owned; null selects default state.
        if unsafe {
            libc::fcopyfile(
                source.as_raw_fd(),
                destination.as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_METADATA,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        destination.set_modified(modified)?;
        let mut attributes = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: libc::ATTR_CMN_CRTIME,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        };
        let mut birth = libc::timespec {
            tv_sec: metadata.st_birthtime(),
            tv_nsec: metadata.st_birthtime_nsec(),
        };
        // fcopyfile's STAT does not preserve birth time on every filesystem.
        // SAFETY: the attribute bitmap describes exactly one native timespec.
        if unsafe {
            libc::fsetattrlist(
                destination.as_raw_fd(),
                (&raw mut attributes).cast(),
                (&raw mut birth).cast(),
                size_of::<libc::timespec>(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // Flags such as hidden/immutable must not disappear on safe-save.
        // SAFETY: destination owns the descriptor and flags came from stat.
        if unsafe { libc::fchflags(destination.as_raw_fd(), metadata.st_flags()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "linux")]
    copy_xattrs(source, destination)?;
    destination.set_permissions(metadata.permissions())
}

#[cfg(target_os = "linux")]
fn copy_xattrs(source: &File, destination: &File) -> io::Result<()> {
    use rustix::fs::{XattrFlags, fgetxattr, flistxattr, fremovexattr, fsetxattr};
    let size = match flistxattr(source, &mut [0u8; 0][..]) {
        Ok(size) => size,
        Err(rustix::io::Errno::NOTSUP) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut names = vec![0; size];
    let length = flistxattr(source, names.as_mut_slice())?;
    let names = &names[..length];
    // A directory's current default ACL may differ from the one under which
    // the source was created. Never retain extra inherited permissions.
    let size = flistxattr(destination, &mut [0u8; 0][..])?;
    let mut inherited = vec![0; size];
    let length = flistxattr(destination, inherited.as_mut_slice())?;
    for name in inherited[..length]
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
    {
        if !names.split(|b| *b == 0).any(|source| source == name) {
            let name = std::ffi::CString::new(name).map_err(|_| io::ErrorKind::InvalidData)?;
            fremovexattr(destination, &name)?;
        }
    }
    for name in names.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let name = std::ffi::CString::new(name).map_err(|_| io::ErrorKind::InvalidData)?;
        let size = fgetxattr(source, &name, &mut [0u8; 0][..])?;
        let mut value = vec![0; size];
        let length = fgetxattr(source, &name, value.as_mut_slice())?;
        fsetxattr(destination, &name, &value[..length], XattrFlags::empty())?;
    }
    Ok(())
}

#[cfg(unix)]
fn rename(
    transaction: &EditTransaction,
    pending: &std::ffi::OsStr,
    _: &File,
    replacing: bool,
) -> io::Result<()> {
    use rustix::fs::{RenameFlags, renameat_with};
    let parent = &transaction.parent;
    let flags = if replacing {
        RenameFlags::empty()
    } else {
        RenameFlags::NOREPLACE
    };
    match renameat_with(parent, pending, parent, &transaction.name, flags) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::NOTSUP) => {
            rename_fallback(transaction, pending, replacing)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn rename_fallback(
    transaction: &EditTransaction,
    pending: &std::ffi::OsStr,
    replacing: bool,
) -> io::Result<()> {
    let parent = &transaction.parent;
    if replacing {
        Ok(rustix::fs::renameat(
            parent,
            pending,
            parent,
            &transaction.name,
        )?)
    } else {
        // linkat publishes exclusively even without extended rename.
        // Pending's destructor removes the second link afterward.
        Ok(rustix::fs::linkat(
            parent,
            pending,
            parent,
            &transaction.name,
            rustix::fs::AtFlags::empty(),
        )?)
    }
}

#[cfg(windows)]
#[derive(PartialEq, Eq)]
struct Signature {
    volume: u64,
    identity: [u8; 16],
    size: u64,
    modified: i64,
    changed: i64,
    attributes: u32,
}

#[cfg(windows)]
fn windows_information(
    file: &File,
) -> io::Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns the handle; information is a writable output structure.
    crate::windows::ok(unsafe {
        GetFileInformationByHandle(file.as_raw_handle(), &raw mut information)
    })?;
    Ok(information)
}

#[cfg(windows)]
fn signature(file: &File) -> io::Result<Signature> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FILE_ID_INFO, FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut identity = FILE_ID_INFO::default();
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: both output buffers have their advertised type and size; file owns the handle.
    unsafe {
        crate::windows::ok(GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut identity).cast(),
            u32::try_from(size_of::<FILE_ID_INFO>()).expect("fixed structure size"),
        ))?;
        crate::windows::ok(GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&raw mut basic).cast(),
            u32::try_from(size_of::<FILE_BASIC_INFO>()).expect("fixed structure size"),
        ))?;
    }
    Ok(Signature {
        volume: identity.VolumeSerialNumber,
        identity: identity.FileId.Identifier,
        size: file.metadata()?.len(),
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
        attributes: basic.FileAttributes,
    })
}

#[cfg(windows)]
fn stage_windows(transaction: &EditTransaction, name: &std::ffi::OsStr) -> io::Result<File> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SetSecurityDescriptorControl,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        COPY_FILE_COPY_SYMLINK, CREATE_NEW, CopyFileExW, CreateFileW, DELETE,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE, WRITE_DAC, WRITE_OWNER,
    };
    let destination = transaction.directory.join(name);
    let target = crate::windows::wide(destination.as_os_str());
    let mut descriptor = std::ptr::null_mut();
    if let Some((source, _)) = &transaction.source {
        // SAFETY: source owns its handle; the returned descriptor is freed below.
        let status = unsafe {
            GetSecurityInfo(
                source.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status.cast_signed()));
        }
        // The temporary file must not gain new ACEs from a directory whose
        // inheritance policy changed since the original file was created.
        // SAFETY: descriptor is the owned, mutable result of GetSecurityInfo.
        let protected = crate::windows::ok(unsafe {
            SetSecurityDescriptorControl(descriptor, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
        });
        if let Err(error) = protected {
            // SAFETY: GetSecurityInfo allocated the descriptor with LocalAlloc.
            unsafe {
                LocalFree(descriptor);
            }
            return Err(error);
        }
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("fixed structure size"),
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // Denying delete sharing pins the exclusively created staging name during
    // CopyFileEx. Its initial access policy is the source's, before any bytes.
    // SAFETY: path and descriptor remain valid through CreateFileW; the handle is checked.
    let opened = crate::windows::owned(unsafe {
        CreateFileW(
            target.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | DELETE | WRITE_DAC | WRITE_OWNER,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &raw const attributes,
            CREATE_NEW,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    });
    if !descriptor.is_null() {
        // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
        unsafe {
            LocalFree(descriptor);
        }
    }
    let file = File::from(opened?);
    if transaction.source.is_some() {
        let source =
            crate::windows::wide(transaction.directory.join(&transaction.name).as_os_str());
        // CopyFileEx retains attributes, EAs and alternate streams. The destination
        // is already exclusively owned and cannot be renamed or replaced here.
        // SAFETY: paths are terminated and alive; optional callback/state are null.
        let result = crate::windows::ok(unsafe {
            CopyFileExW(
                source.as_ptr(),
                target.as_ptr(),
                None,
                std::ptr::null(),
                std::ptr::null_mut(),
                COPY_FILE_COPY_SYMLINK,
            )
        });
        if let Err(error) = result {
            drop(file);
            remove_pending(&destination);
            return Err(error);
        }
    }
    Ok(file)
}

#[cfg(windows)]
fn remove_pending(path: &Path) {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_READONLY, SetFileAttributesW};
    if let Ok(metadata) = fs::symlink_metadata(path) {
        let attributes = metadata.file_attributes();
        if attributes & FILE_ATTRIBUTE_READONLY != 0 {
            let path = crate::windows::wide(path.as_os_str());
            // SAFETY: this is an unpublished, owned staging file and the path is terminated.
            unsafe {
                SetFileAttributesW(path.as_ptr(), attributes & !FILE_ATTRIBUTE_READONLY);
            }
        }
        let _ = fs::remove_file(path);
    }
}

#[cfg(windows)]
fn preserve_metadata(source: &File, destination: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorControl,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::SetFileTime;
    let creation = windows_information(source)?.ftCreationTime;
    // SAFETY: destination owns the handle; null preserves access/write times.
    crate::windows::ok(unsafe {
        SetFileTime(
            destination.as_raw_handle(),
            &raw const creation,
            std::ptr::null(),
            std::ptr::null(),
        )
    })?;
    let (mut owner, mut group, mut dacl, mut descriptor) = (
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    let information =
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    // SAFETY: source owns its handle; output pointers receive memory owned by the returned descriptor.
    let status = unsafe {
        GetSecurityInfo(
            source.as_raw_handle(),
            SE_FILE_OBJECT,
            information,
            &raw mut owner,
            &raw mut group,
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status.cast_signed()));
    }
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor remains valid until LocalFree below, including all interior pointers.
    let result = unsafe {
        crate::windows::ok(GetSecurityDescriptorControl(
            descriptor,
            &raw mut control,
            &raw mut revision,
        ))
        .and_then(|()| {
            let protection = if control & SE_DACL_PROTECTED != 0 {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
            let status = SetSecurityInfo(
                destination.as_raw_handle(),
                SE_FILE_OBJECT,
                information | protection,
                owner,
                group,
                dacl,
                std::ptr::null(),
            );
            if status == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(status.cast_signed()))
            }
        })
    };
    // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
    unsafe {
        LocalFree(descriptor);
    }
    result
}

#[cfg(windows)]
fn rename(
    transaction: &EditTransaction,
    _: &std::ffi::OsStr,
    file: &File,
    replacing: bool,
) -> io::Result<()> {
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfoEx, SetFileInformationByHandle,
    };
    let name: Vec<u16> = transaction
        .directory
        .join(&transaction.name)
        .as_os_str()
        .encode_wide()
        .collect();
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let bytes = offset + name.len() * 2;
    let length = u32::try_from(bytes).map_err(|_| io::ErrorKind::InvalidInput)?;
    let mut buffer = vec![0u64; bytes.max(size_of::<FILE_RENAME_INFO>()).div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: u64 storage aligns the structure, has room for its flexible UTF-16
    // tail and stays alive until the synchronous call completes.
    unsafe {
        (*info).Anonymous.Flags = if replacing { 0x1 | 0x2 } else { 0x2 }; // REPLACE_IF_EXISTS | POSIX_SEMANTICS
        (*info).FileNameLength = u32::try_from(name.len() * 2).expect("checked buffer length");
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        crate::windows::ok(SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfoEx,
            info.cast(),
            length,
        ))
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Self {
            let path = std::env::temp_dir().join(temporary_name());
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn target(&self) -> PathBuf {
            self.0.join("target")
        }

        fn capture(&self) -> EditTransaction {
            EditTransaction::capture(&self.target(), 1 << 20).unwrap()
        }

        fn only_target_remains(&self) {
            let names: Vec<_> = fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, vec![OsString::from("target")]);
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn publication_replaces_the_name_without_truncating_open_readers() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let mut reader = File::open(work.target()).unwrap();
        let receipt = work.capture().publish(b"replacement", false).unwrap();
        assert_eq!(
            receipt,
            EditReceipt {
                visible: true,
                durability: Durability::NotRequested
            }
        );
        let mut old = String::new();
        reader.read_to_string(&mut old).unwrap();
        assert_eq!(old, "original");
        assert_eq!(fs::read(work.target()).unwrap(), b"replacement");
        work.only_target_remains();
    }

    #[test]
    fn a_concurrent_in_place_writer_is_a_conflict() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let transaction = work.capture();
        let path = work.target();
        std::thread::spawn(move || fs::write(path, b"external").unwrap())
            .join()
            .unwrap();
        assert!(matches!(
            transaction.publish(b"ours", false),
            Err(EditError::Conflict(EditConflict::Changed))
        ));
        assert_eq!(fs::read(work.target()).unwrap(), b"external");
        work.only_target_remains();
    }

    #[test]
    fn replacement_with_identical_bytes_is_still_a_conflict() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let transaction = work.capture();
        work.capture().publish(b"original", false).unwrap();
        assert!(matches!(
            transaction.publish(b"ours", false),
            Err(EditError::Conflict(EditConflict::Changed))
        ));
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
    }

    #[test]
    fn competing_creators_never_overwrite_each_other() {
        let work = Workspace::new();
        let first = work.capture();
        let second = work.capture();
        assert!(first.original().is_none());
        first.publish(b"first", false).unwrap();
        assert!(matches!(
            second.publish(b"second", false),
            Err(EditError::Conflict(EditConflict::Appeared))
        ));
        assert_eq!(fs::read(work.target()).unwrap(), b"first");
        work.only_target_remains();
    }

    #[test]
    fn deletion_during_edit_does_not_recreate_the_target() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let transaction = work.capture();
        fs::remove_file(work.target()).unwrap();
        assert!(matches!(
            transaction.publish(b"ours", false),
            Err(EditError::Conflict(EditConflict::Changed))
        ));
        assert_eq!(fs::read_dir(&work.0).unwrap().count(), 0);
    }

    #[test]
    fn rejection_and_abandonment_leave_the_original_untouched() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        assert!(matches!(
            EditTransaction::capture(&work.target(), 2),
            Err(EditError::TooLarge)
        ));
        drop(work.capture());
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
        assert!(matches!(
            EditTransaction::capture(&work.0, 16),
            Err(EditError::NotRegular)
        ));
    }

    #[test]
    fn hard_links_are_never_silently_severed() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let alias = work.0.join("alias");
        fs::hard_link(work.target(), &alias).unwrap();
        assert!(matches!(
            EditTransaction::capture(&work.target(), 16),
            Err(EditError::MultipleLinks)
        ));
        assert_eq!(fs::read(alias).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn special_files_and_symlinks_are_rejected_without_blocking() {
        use std::os::unix::fs::symlink;
        let work = Workspace::new();
        let fifo = work.0.join("fifo");
        let encoded = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: encoded is a terminated pathname and remains alive through the call.
        assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
        symlink(&fifo, work.target()).unwrap();
        for path in [&fifo, &work.target(), Path::new("/dev/null")] {
            assert!(matches!(
                EditTransaction::capture(path, 32),
                Err(EditError::NotRegular)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn mode_ownership_and_extended_attributes_survive_publication() {
        use rustix::fs::{XattrFlags, fgetxattr, fsetxattr};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        fs::set_permissions(work.target(), fs::Permissions::from_mode(0o751)).unwrap();
        let source = File::open(work.target()).unwrap();
        let before = source.metadata().unwrap();
        let attribute = c"user.a-edit";
        fsetxattr(&source, attribute, b"metadata", XattrFlags::empty()).unwrap();
        work.capture().publish(b"new", false).unwrap();
        let destination = File::open(work.target()).unwrap();
        let after = destination.metadata().unwrap();
        assert_eq!(
            (before.mode(), before.uid(), before.gid()),
            (after.mode(), after.uid(), after.gid())
        );
        #[cfg(target_vendor = "apple")]
        assert_eq!(before.created().unwrap(), after.created().unwrap());
        let mut value = [0; 32];
        let count = fgetxattr(&destination, attribute, &mut value[..]).unwrap();
        assert_eq!(&value[..count], b"metadata");
    }

    #[cfg(unix)]
    #[test]
    fn durability_failures_report_that_the_new_content_is_visible() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let receipt = work
            .capture()
            .publish_with_sync(b"new", true, |_| {
                Err(io::Error::other("injected directory flush failure"))
            })
            .unwrap();
        assert!(receipt.visible);
        assert!(matches!(receipt.durability, Durability::Failed(_)));
        assert_eq!(fs::read(work.target()).unwrap(), b"new");
        let receipt = work.capture().publish(b"durable", true).unwrap();
        assert_eq!(receipt.durability, Durability::Confirmed);
        work.only_target_remains();
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_parent_cannot_redirect_a_transaction() {
        let work = Workspace::new();
        let parent = work.0.join("parent");
        let moved = work.0.join("moved");
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("file"), b"original").unwrap();
        let transaction = EditTransaction::capture(&parent.join("file"), 32).unwrap();
        fs::rename(&parent, &moved).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("file"), b"unrelated").unwrap();
        transaction.publish(b"edited", false).unwrap();
        assert_eq!(fs::read(parent.join("file")).unwrap(), b"unrelated");
        assert_eq!(fs::read(moved.join("file")).unwrap(), b"edited");
    }

    #[cfg(unix)]
    #[test]
    fn fallback_publication_is_exclusive_even_after_the_last_check() {
        let work = Workspace::new();
        let transaction = work.capture();
        let (name, mut file) = transaction.stage().unwrap();
        let pending = Pending {
            transaction: &transaction,
            name,
        };
        file.write_all(b"ours").unwrap();
        transaction.revalidate().unwrap();
        fs::write(work.target(), b"theirs").unwrap();
        assert_eq!(
            rename_fallback(&transaction, &pending.name, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(work.target()).unwrap(), b"theirs");
        drop(pending);
        work.only_target_remains();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn posix_acls_are_preserved_without_adding_inherited_access() {
        use rustix::fs::{XattrFlags, fgetxattr, fsetxattr};
        // Linux's POSIX ACL xattr ABI: version, then (tag, permissions, id).
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (1u16, 6u16, u32::MAX),
            (2, 4, 42),
            (4, 4, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(permissions.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let source = File::open(work.target()).unwrap();
        fsetxattr(
            &source,
            c"system.posix_acl_access",
            &acl,
            XattrFlags::empty(),
        )
        .unwrap();
        work.capture().publish(b"new", false).unwrap();
        let mut copied = [0; 128];
        let count = fgetxattr(
            File::open(work.target()).unwrap(),
            c"system.posix_acl_access",
            &mut copied[..],
        )
        .unwrap();
        assert_eq!(&copied[..count], acl);

        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        fsetxattr(
            File::open(&work.0).unwrap(),
            c"system.posix_acl_default",
            &acl,
            XattrFlags::empty(),
        )
        .unwrap();
        work.capture().publish(b"new", false).unwrap();
        assert_eq!(
            fgetxattr(
                File::open(work.target()).unwrap(),
                c"system.posix_acl_access",
                &mut copied[..]
            )
            .unwrap_err(),
            rustix::io::Errno::NODATA
        );
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn macos_access_control_entries_survive_safe_save() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone deny execute"])
            .arg(work.target())
            .status()
            .unwrap();
        assert!(status.success());
        let entries = || {
            let output = std::process::Command::new("/bin/ls")
                .arg("-le")
                .arg(work.target())
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let before = entries();
        assert!(before.contains("deny execute"));
        work.capture().publish(b"new", false).unwrap();
        assert_eq!(entries(), before);
    }

    #[cfg(windows)]
    #[test]
    fn protected_windows_dacls_survive_safe_save() {
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
        };
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let security = crate::windows::Security::user_only().unwrap();
        let path = crate::windows::wide(work.target().as_os_str());
        // SAFETY: security owns the descriptor and path is terminated.
        crate::windows::ok(unsafe {
            SetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                security.descriptor(),
            )
        })
        .unwrap();
        let entries = || {
            let output = std::process::Command::new("icacls.exe")
                .arg(work.target())
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let before = entries();
        work.capture().publish(b"new", false).unwrap();
        assert_eq!(entries(), before);
    }

    #[cfg(windows)]
    #[test]
    fn alternate_streams_survive_while_stream_targets_are_rejected() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let stream = work.0.join("target:Zone.Identifier");
        fs::write(&stream, b"[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();
        assert!(matches!(
            EditTransaction::capture(&stream, 128),
            Err(EditError::NotRegular)
        ));
        work.capture().publish(b"new", false).unwrap();
        assert_eq!(fs::read(stream).unwrap(), b"[ZoneTransfer]\r\nZoneId=3\r\n");
        work.only_target_remains();
    }

    #[test]
    fn simultaneous_private_saves_never_share_or_unlink_staging_files() {
        let work = Workspace::new();
        let sentinel = work.0.join(format!(".target.{}", std::process::id()));
        fs::write(&sentinel, b"unrelated").unwrap();
        let gate = std::sync::Arc::new(std::sync::Barrier::new(8));
        let writers: Vec<_> = (0u8..8)
            .map(|byte| {
                let gate = gate.clone();
                let path = work.target();
                std::thread::spawn(move || {
                    gate.wait();
                    super::super::write_private(&path, &[byte; 8192]).unwrap();
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let bytes = fs::read(work.target()).unwrap();
        assert_eq!(bytes.len(), 8192);
        assert!(bytes.iter().all(|byte| *byte == bytes[0]));
        assert_eq!(fs::read(&sentinel).unwrap(), b"unrelated");
        fs::remove_file(sentinel).unwrap();
        work.only_target_remains();
    }
}
