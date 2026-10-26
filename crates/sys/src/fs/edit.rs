//! Safe-save publication. Final-component links and multiply linked files are
//! rejected: replacing either would silently change which object is edited.
//! Publication is one atomic native rename, never evacuation or unlink. Exchange
//! verifies displaced identity, bytes and metadata and swaps back on conflict.
//! Receipts expose the non-exchange path race and writes through old descriptors.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::Persistence;
#[cfg(windows)]
use super::staging::{Creation, ObjectKind};
use super::staging::{Directory, Staged};

/// Another process holding the target open without sharing is a conflict.
fn busy(error: io::Error) -> EditError {
    #[cfg(windows)]
    if error.raw_os_error() == Some(32) {
        // ERROR_SHARING_VIOLATION
        return EditError::Conflict(EditConflict::Busy);
    }
    error.into()
}

// Checkpoints disappear entirely from non-test builds.
macro_rules! publication_checkpoint {
    ($step:literal) => {
        #[cfg(test)]
        publication_step($step);
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditConflict {
    Busy,
    Changed,
    Appeared,
    /// The swap back did not restore the captured state; the displaced file
    /// is never deleted and stays at `retained` for recovery.
    RollbackFailed {
        retained: PathBuf,
    },
}

impl std::fmt::Display for EditConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("the file is in use"),
            Self::Changed => f.write_str("the file changed"),
            Self::Appeared => f.write_str("the file appeared"),
            Self::RollbackFailed { retained } => write!(
                f,
                "restoring the changed file failed; its displaced version is kept at {}",
                retained.display()
            ),
        }
    }
}

#[derive(Debug)]
pub enum EditError {
    Conflict(EditConflict),
    NotRegular,
    MultipleLinks,
    TooLarge,
    Cancelled,
    UnsupportedPublication,
    Io(io::Error),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(reason) => {
                write!(f, "edit conflict: {reason}; inspect the file and retry")
            }
            Self::NotRegular => {
                f.write_str("edit requires a regular file; symbolic links are rejected")
            }
            Self::MultipleLinks => f.write_str("edit refuses to break multiple hard links"),
            Self::TooLarge => f.write_str("file exceeds byte limit"),
            Self::Cancelled => f.write_str("edit cancelled"),
            Self::UnsupportedPublication => {
                f.write_str("filesystem does not support safe edit publication")
            }
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for EditError {}

impl From<io::Error> for EditError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::Interrupted => Self::Cancelled,
            io::ErrorKind::FileTooLarge => Self::TooLarge,
            io::ErrorKind::Unsupported => Self::UnsupportedPublication,
            _ => Self::Io(error),
        }
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

/// The strongest conflict check completed by the filesystem, separately from durability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationProtection {
    Unchanged,
    ExclusiveCreate,
    ExchangeVerified,
    RevalidatedAtomicReplace,
}

/// Exclusions apply after the last complete identity/content/metadata check.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationLimitations {
    /// Any change to the target (content, a path replacement, mode, xattrs,
    /// ACLs or attributes) between the final check and a non-exchange rename
    /// is overwritten.
    pub change_after_check: bool,
    /// A descriptor still open on the old Unix inode can write to it after
    /// publication; those writes never reach the published file. Windows
    /// denies write sharing from capture, so this cannot happen there.
    pub writes_to_old_inode: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditReceipt {
    pub visible: bool,
    pub durability: Durability,
    pub protection: PublicationProtection,
    pub limitations: PublicationLimitations,
}

/// Owns the captured source and parent until publication or abort.
pub struct EditTransaction {
    parent: File,
    #[cfg(windows)]
    directory: PathBuf,
    observed_parent: PathBuf,
    name: OsString,
    source: Option<CapturedSource>,
    limit: Option<usize>,
}

struct CapturedSource {
    file: File,
    signature: Signature,
    original: File,
    #[cfg(windows)]
    stream_bytes: u64,
}

impl EditTransaction {
    pub fn capture(path: &Path, limit: Option<usize>) -> Result<Self, EditError> {
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
        let observed_parent = std::path::absolute(directory)?;
        let directory = directory.canonicalize()?;
        let parent = open_parent(&directory)?;
        let mut transaction = Self {
            parent,
            #[cfg(windows)]
            directory,
            observed_parent,
            name,
            source: None,
            limit,
        };
        let mut file = match transaction.open_source() {
            Ok(file) => file,
            Err(EditError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(transaction);
            }
            Err(error) => return Err(error),
        };
        let signature = signature(&file)?;
        #[cfg(target_vendor = "apple")]
        mac_xattrs(&file, file.metadata()?.len(), limit.unwrap_or(usize::MAX))?;
        #[cfg(target_os = "linux")]
        linux_xattrs(&file, file.metadata()?.len(), limit.unwrap_or(usize::MAX))?;
        #[cfg(windows)]
        let stream_bytes = stream_bytes(&file, limit.unwrap_or(usize::MAX))?;
        let mut bytes = tempfile::tempfile()?;
        let count = io::copy(
            &mut (&mut file).take(limit.map_or(u64::MAX, |limit| (limit as u64).saturating_add(1))),
            &mut bytes,
        )?;
        if limit.is_some_and(|limit| count > limit as u64) {
            return Err(EditError::TooLarge);
        }
        if signature != self::signature(&file)? {
            return Err(EditError::Conflict(EditConflict::Changed));
        }
        transaction.source = Some(CapturedSource {
            file,
            signature,
            original: bytes,
            #[cfg(windows)]
            stream_bytes,
        });
        Ok(transaction)
    }

    pub fn original(&self) -> Option<&File> {
        self.source.as_ref().map(|source| &source.original)
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
        let file =
            super::staging::relative(&self.parent, &self.name, ObjectKind::File, Creation::Open)
                .map_err(|error| {
                    if error.raw_os_error() == Some(32) {
                        EditError::Conflict(EditConflict::Busy)
                    } else if error.raw_os_error() == Some(267)
                        || error.kind() == io::ErrorKind::IsADirectory
                    {
                        EditError::NotRegular
                    } else {
                        EditError::Io(error)
                    }
                })?;
        validate(&file)?;
        Ok(file)
    }

    fn revalidate(&self) -> Result<(), EditError> {
        let parent = open_parent(&self.observed_parent)
            .map_err(|_| EditError::Conflict(EditConflict::Changed))?;
        if !same_object(&self.parent, &parent)? {
            return Err(EditError::Conflict(EditConflict::Changed));
        }
        match (&self.source, self.open_source()) {
            (None, Err(EditError::Io(error))) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            (None, _) => Err(EditError::Conflict(EditConflict::Appeared)),
            (
                Some(CapturedSource {
                    file: source,
                    signature: before,
                    original,
                    ..
                }),
                Ok(mut current),
            ) => {
                if *before != signature(&current)? || *before != signature(source)? {
                    return Err(EditError::Conflict(EditConflict::Changed));
                }
                check_bytes(&mut current, original)?;
                if *before != signature(&current)? {
                    return Err(EditError::Conflict(EditConflict::Changed));
                }
                Ok(())
            }
            (Some(_), Err(_)) => Err(EditError::Conflict(EditConflict::Changed)),
        }
    }

    /// Consuming the transaction ensures an abandoned staging file is reclaimed.
    /// A post-rename flush failure is a visible receipt, never an ambiguous error.
    /// Windows publication promises visibility only; durable mode is rejected
    /// before writing because there is no documented directory flush barrier.
    /// The cancellation probe receives aggregate bytes copied/written so far;
    /// it is checked by native stream callbacks and immediately before commit.
    pub fn publish(
        self,
        bytes: &[u8],
        persistence: Persistence,
        should_cancel: &(dyn Fn(u64) -> bool + Sync),
    ) -> Result<EditReceipt, EditError> {
        self.publish_reader(
            &mut io::Cursor::new(bytes),
            bytes.len() as u64,
            persistence,
            should_cancel,
        )
    }

    /// Streams an exact-length result through the same atomic publication protocol.
    pub fn publish_reader(
        self,
        reader: &mut dyn Read,
        length: u64,
        persistence: Persistence,
        should_cancel: &(dyn Fn(u64) -> bool + Sync),
    ) -> Result<EditReceipt, EditError> {
        self.publish_stream(
            reader,
            length,
            persistence,
            File::sync_all,
            should_cancel,
            true,
            || {},
        )
    }

    #[cfg(test)]
    fn publish_with_sync(
        self,
        bytes: &[u8],
        persistence: Persistence,
        sync_parent: impl FnOnce(&File) -> io::Result<()>,
        should_cancel: &(dyn Fn(u64) -> bool + Sync),
        exchange: bool,
        before_commit: impl FnOnce(),
    ) -> Result<EditReceipt, EditError> {
        self.publish_stream(
            &mut io::Cursor::new(bytes),
            bytes.len() as u64,
            persistence,
            sync_parent,
            should_cancel,
            exchange,
            before_commit,
        )
    }

    #[expect(clippy::too_many_arguments, reason = "publication test checkpoints")]
    fn publish_stream(
        self,
        reader: &mut dyn Read,
        length: u64,
        persistence: Persistence,
        sync_parent: impl FnOnce(&File) -> io::Result<()>,
        should_cancel: &(dyn Fn(u64) -> bool + Sync),
        exchange: bool,
        before_commit: impl FnOnce(),
    ) -> Result<EditReceipt, EditError> {
        #[cfg(windows)]
        if persistence == Persistence::Durable {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "edit publication does not support durability on Windows",
            )
            .into());
        }
        let metadata_content = self.output_size(length)?;
        #[cfg(windows)]
        let _ = metadata_content;
        #[cfg(unix)]
        let mut copied = 0;
        #[cfg(windows)]
        let mut copied = self.source.as_ref().map_or(0, |source| source.stream_bytes);
        check_cancel(should_cancel, 0)?;
        self.revalidate()?;
        #[cfg(target_vendor = "apple")]
        if let Some(CapturedSource { file: source, .. }) = &self.source {
            mac_xattrs(
                source,
                metadata_content as u64,
                self.limit.unwrap_or(usize::MAX),
            )?;
        }
        let mut staged = self
            .stage(should_cancel)
            .map_err(|error| match error.kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::FileTooLarge => EditError::from(error),
                io::ErrorKind::ResourceBusy => EditError::Conflict(EditConflict::Changed),
                _ => self.revalidate().err().unwrap_or(EditError::Io(error)),
            })?;
        publication_checkpoint!("staged");
        staged.file.rewind()?;
        copy_content(reader, &mut staged.file, length, &mut copied, should_cancel)?;
        staged.file.set_len(length)?;
        publication_checkpoint!("written");
        check_cancel(should_cancel, copied)?;
        #[cfg(unix)]
        self.copy_metadata(&staged.file, metadata_content, &mut copied, should_cancel)?;
        #[cfg(windows)]
        if let Some(CapturedSource { file: source, .. }) = &self.source {
            preserve_metadata(source, &staged.file)?;
        }
        publication_checkpoint!("metadata");
        if persistence == Persistence::Durable {
            staged.file.sync_all()?;
        }
        publication_checkpoint!("file_sync");
        self.revalidate_staged(&staged.file)?;
        check_cancel(should_cancel, copied)?;
        publication_checkpoint!("final_check");
        before_commit();
        let protection = self.commit(
            &mut staged,
            exchange,
            || check_cancel(should_cancel, copied),
            || {},
            || {},
        )?;
        let durability = if persistence == Persistence::Durable {
            match sync_parent(&self.parent) {
                Ok(()) => Durability::Confirmed,
                Err(error) => Durability::Failed(error.to_string()),
            }
        } else {
            Durability::NotRequested
        };
        publication_checkpoint!("parent_sync");
        #[cfg(test)]
        drop(staged);
        publication_checkpoint!("cleanup");
        Ok(EditReceipt {
            visible: true,
            durability,
            limitations: PublicationLimitations {
                change_after_check: protection == PublicationProtection::RevalidatedAtomicReplace,
                writes_to_old_inode: cfg!(unix) && self.source.is_some(),
            },
            protection,
        })
    }

    fn output_size(&self, length: u64) -> Result<usize, EditError> {
        if self.limit.is_some_and(|limit| length > limit as u64) {
            return Err(EditError::TooLarge);
        }
        let original = self
            .source
            .as_ref()
            .map(|source| source.original.metadata().map(|m| m.len()))
            .transpose()?
            .unwrap_or(0);
        #[cfg(windows)]
        if self.limit.is_some_and(|limit| {
            self.source
                .as_ref()
                .map_or(0, |source| source.stream_bytes)
                .saturating_sub(original)
                .saturating_add(length)
                > limit as u64
        }) {
            return Err(EditError::TooLarge);
        }
        usize::try_from(original.max(length)).map_err(|_| EditError::TooLarge)
    }

    #[cfg(unix)]
    fn copy_metadata(
        &self,
        staged: &File,
        metadata_content: usize,
        copied: &mut u64,
        should_cancel: &(dyn Fn(u64) -> bool + Sync),
    ) -> Result<(), EditError> {
        if let Some(CapturedSource { file: source, .. }) = &self.source {
            #[cfg(target_vendor = "apple")]
            copy_mac_xattrs(
                source,
                staged,
                metadata_content,
                self.limit.unwrap_or(usize::MAX),
                copied,
                should_cancel,
            )?;
            #[cfg(target_os = "linux")]
            copy_xattrs(
                source,
                staged,
                metadata_content,
                self.limit.unwrap_or(usize::MAX),
                copied,
                should_cancel,
            )?;
            preserve_metadata(source, staged)?;
        }
        Ok(())
    }

    fn commit(
        &self,
        staged: &mut Staged<'_>,
        exchange: bool,
        cancel: impl Fn() -> io::Result<()>,
        after_exchange: impl FnOnce(),
        before_rollback: impl FnOnce(),
    ) -> Result<PublicationProtection, EditError> {
        cancel()?;
        publication_checkpoint!("before_replace");
        #[cfg(windows)]
        let _ = (exchange, before_rollback);
        #[cfg(unix)]
        if let Some(source) = &self.source
            && exchange
            && staged.exchange(&self.name).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    EditError::Conflict(EditConflict::Changed)
                } else {
                    EditError::from(error)
                }
            })?
        {
            after_exchange();
            publication_checkpoint!("after_replace");
            let checked = staged
                .displaced()
                .map_err(EditError::from)
                .and_then(|mut displaced| {
                    source.verify_displaced(&mut displaced, &staged.file, self.limit)
                });
            publication_checkpoint!("displaced_checked");
            if checked.is_err() {
                before_rollback();
                publication_checkpoint!("before_rollback");
                // A name replaced again since the first exchange swaps that
                // newer file into staging: it is kept, never reclaimed.
                if !staged.exchange(&self.name).unwrap_or(false) || !staged.holds_staged() {
                    return Err(EditError::Conflict(EditConflict::RollbackFailed {
                        retained: self
                            .observed_parent
                            .join(&staged.directory.name)
                            .join(super::staging::CONTENT),
                    }));
                }
                publication_checkpoint!("after_rollback");
                return Err(EditError::Conflict(EditConflict::Changed));
            }
            staged.reclaim();
            return Ok(PublicationProtection::ExchangeVerified);
        }
        cancel()?;
        let protection = if self.source.is_some() {
            staged.replace(&self.name).map_err(busy)?;
            PublicationProtection::RevalidatedAtomicReplace
        } else {
            match staged.publish(&self.name) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(EditError::Conflict(EditConflict::Appeared));
                }
                Err(error) => return Err(busy(error)),
            }
            PublicationProtection::ExclusiveCreate
        };
        after_exchange();
        publication_checkpoint!("after_replace");
        Ok(protection)
    }

    fn revalidate_staged(&self, staged: &File) -> Result<(), EditError> {
        if let Some(CapturedSource { file: source, .. }) = &self.source
            && !preserved_metadata_matches(source, staged, self.limit.unwrap_or(usize::MAX))?
        {
            return Err(EditError::Conflict(EditConflict::Changed));
        }
        self.revalidate()
    }

    fn stage(&self, should_cancel: &(dyn Fn(u64) -> bool + Sync)) -> io::Result<Staged<'_>> {
        check_cancel(should_cancel, 0)?;
        #[cfg(unix)]
        let path = &self.observed_parent;
        #[cfg(windows)]
        let path = &self.directory;
        let directory = Directory::create(&self.parent, path)?;
        #[cfg(unix)]
        return Staged::create(directory);
        #[cfg(windows)]
        Staged::copied(directory, |directory| {
            stage_windows(self, directory, should_cancel)
        })
    }
}

impl CapturedSource {
    #[cfg(unix)]
    fn verify_displaced(
        &self,
        displaced: &mut File,
        snapshot: &File,
        limit: Option<usize>,
    ) -> Result<(), EditError> {
        let observed = signature(displaced)?;
        if moved_signature(self.signature) != moved_signature(observed) {
            return Err(EditError::Conflict(EditConflict::Changed));
        }
        displaced.rewind()?;
        check_bytes(displaced, &self.original)?;
        if !preserved_metadata_matches(displaced, snapshot, limit.unwrap_or(usize::MAX))?
            || signature(displaced)? != observed
        {
            return Err(EditError::Conflict(EditConflict::Changed));
        }
        Ok(())
    }
}

fn copy_content(
    reader: &mut dyn Read,
    output: &mut File,
    mut remaining: u64,
    copied: &mut u64,
    should_cancel: &(dyn Fn(u64) -> bool + Sync),
) -> io::Result<()> {
    let mut buffer = vec![0; 65536];
    while remaining > 0 {
        check_cancel(should_cancel, *copied)?;
        let count =
            usize::try_from(remaining.min(buffer.len() as u64)).expect("bounded buffer length");
        reader.read_exact(&mut buffer[..count])?;
        output.write_all(&buffer[..count])?;
        remaining -= count as u64;
        *copied += count as u64;
    }
    Ok(())
}

fn check_bytes(current: &mut File, mut original: &File) -> Result<(), EditError> {
    original.rewind()?;
    let mut buffer = vec![0; 65536];
    let mut expected = vec![0; 65536];
    loop {
        let count = current.read(&mut buffer)?;
        if count == 0 {
            if original.read(&mut expected[..1])? != 0 {
                return Err(EditError::Conflict(EditConflict::Changed));
            }
            return Ok(());
        }
        if original.read_exact(&mut expected[..count]).is_err()
            || buffer[..count] != expected[..count]
        {
            return Err(EditError::Conflict(EditConflict::Changed));
        }
    }
}

#[cfg(unix)]
fn moved_signature(mut signature: Signature) -> Signature {
    // Rename changes ctime itself; identity, mtime, mode and bytes must still match.
    signature.5 = 0;
    signature.6 = 0;
    signature
}

fn check_cancel(should_cancel: &(dyn Fn(u64) -> bool + Sync), copied: u64) -> io::Result<()> {
    if should_cancel(copied) {
        Err(io::Error::new(io::ErrorKind::Interrupted, "edit cancelled"))
    } else {
        Ok(())
    }
}

pub(super) fn same_object(left: &File, right: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (left, right) = (left.metadata()?, right.metadata()?);
        Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
    }
    #[cfg(windows)]
    {
        let (left, right) = (signature(left)?, signature(right)?);
        Ok((left.volume, left.identity) == (right.volume, right.identity))
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

pub(super) fn open_parent(path: &Path) -> io::Result<File> {
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
        use windows_sys::Win32::Foundation::GENERIC_READ;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ADD_FILE, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        // Pin the spelling used by CopyFileEx; publication uses the owned handle.
        fs::OpenOptions::new()
            .access_mode(GENERIC_READ | FILE_ADD_FILE)
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
        // Resource forks use bounded positioned IO before STAT: fcopyfile's
        // descriptor-based fork copying does not deliver per-chunk callbacks.
        // The content edit gets its own modification time.
        // SAFETY: both descriptors remain owned; null selects default state.
        if unsafe {
            libc::fcopyfile(
                source.as_raw_fd(),
                destination.as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_STAT,
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
    destination.set_permissions(metadata.permissions())?;
    #[cfg(target_vendor = "apple")]
    access_acl(Some(source), destination)?;
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn mac_xattrs(source: &File, content: u64, limit: usize) -> io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    // Count names as metadata too; this bounds enumeration and empty attributes.
    let mut remaining = (limit as u64)
        .checked_sub(content)
        .ok_or(io::ErrorKind::FileTooLarge)?;
    // SAFETY: a null output asks only for the required buffer size.
    let length = unsafe { libc::flistxattr(source.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
    let length = mac_count(length)?;
    remaining = remaining
        .checked_sub(length as u64)
        .ok_or(io::ErrorKind::FileTooLarge)?;
    let mut names = vec![0u8; length];
    // SAFETY: names owns the advertised writable byte buffer.
    let length = unsafe {
        libc::flistxattr(
            source.as_raw_fd(),
            names.as_mut_ptr().cast(),
            names.len(),
            0,
        )
    };
    let length = mac_count(length)?;
    if length > names.len() {
        return Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "edit metadata changed",
        ));
    }
    names.truncate(length);
    for name in names.split_inclusive(|byte| *byte == 0) {
        let name =
            std::ffi::CStr::from_bytes_with_nul(name).map_err(|_| io::ErrorKind::InvalidData)?;
        // SAFETY: the name is terminated; null asks for size without reading data.
        let length = unsafe {
            libc::fgetxattr(
                source.as_raw_fd(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };
        remaining = remaining
            .checked_sub(mac_count(length)? as u64)
            .ok_or(io::ErrorKind::FileTooLarge)?;
    }
    Ok(names)
}

#[cfg(target_vendor = "apple")]
fn mac_count(value: libc::ssize_t) -> io::Result<usize> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value.cast_unsigned())
    }
}

#[cfg(target_vendor = "apple")]
fn copy_mac_xattrs(
    source: &File,
    destination: &File,
    content: usize,
    limit: usize,
    copied: &mut u64,
    should_cancel: &(dyn Fn(u64) -> bool + Sync),
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let names = mac_xattrs(source, content as u64, limit)?;
    let mut remaining = limit - content - names.len();
    for name in names.split_inclusive(|byte| *byte == 0) {
        let name =
            std::ffi::CStr::from_bytes_with_nul(name).map_err(|_| io::ErrorKind::InvalidData)?;
        check_cancel(should_cancel, *copied)?;
        // SAFETY: the source and terminated name remain live through the query.
        let size = mac_count(unsafe {
            libc::fgetxattr(
                source.as_raw_fd(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        })?;
        remaining = remaining
            .checked_sub(size)
            .ok_or(io::ErrorKind::FileTooLarge)?;
        let fork = name == c"com.apple.ResourceFork";
        // Only resource forks support positioned xattr IO. Ordinary attributes
        // must fit in one bounded allocation; forks never allocate their size.
        let mut value = vec![0u8; if fork { size.min(65536) } else { size }];
        let mut at = 0usize;
        loop {
            check_cancel(should_cancel, *copied)?;
            let count = (size - at).min(value.len());
            let position = u32::try_from(at).map_err(|_| io::ErrorKind::FileTooLarge)?;
            // SAFETY: the buffer covers count; position is supported for forks
            // and is zero for an ordinary attribute's single operation.
            let read = mac_count(unsafe {
                libc::fgetxattr(
                    source.as_raw_fd(),
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    count,
                    position,
                    0,
                )
            })?;
            if read != count {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "edit metadata changed",
                ));
            }
            // SAFETY: exactly count initialized bytes are supplied to the owned file.
            if unsafe {
                libc::fsetxattr(
                    destination.as_raw_fd(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    count,
                    position,
                    0,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            *copied += count as u64;
            at += count;
            check_cancel(should_cancel, *copied)?;
            if at == size {
                break;
            }
        }
    }
    let _ = remaining;
    Ok(())
}

#[cfg(target_vendor = "apple")]
pub(super) fn access_acl(source: Option<&File>, destination: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn acl_get_fd(fd: libc::c_int) -> *mut std::ffi::c_void;
        fn acl_init(count: libc::c_int) -> *mut std::ffi::c_void;
        fn acl_set_fd(fd: libc::c_int, acl: *mut std::ffi::c_void) -> libc::c_int;
        fn acl_free(acl: *mut std::ffi::c_void) -> libc::c_int;
    }
    // copyfile may return success after an ACL failure. Apply the access policy
    // with checked calls, replacing any ACL inherited by the staging file.
    // SAFETY: source owns its descriptor; the returned ACL is freed below.
    let mut acl = source.map_or(std::ptr::null_mut(), |source| unsafe {
        acl_get_fd(source.as_raw_fd())
    });
    if acl.is_null() {
        let error = io::Error::last_os_error();
        if source.is_some() && error.raw_os_error() != Some(libc::ENOENT) {
            return Err(error);
        }
        // SAFETY: an empty ACL removes inherited access; zero entries is valid.
        acl = unsafe { acl_init(0) };
        if acl.is_null() {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: destination owns its descriptor and acl is live until acl_free.
    let result = if unsafe { acl_set_fd(destination.as_raw_fd(), acl) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    // SAFETY: acl_get_fd or acl_init allocated this object; neither setter owns it.
    unsafe {
        acl_free(acl);
    }
    result
}

/// Establish the runtime directory policy once; ordinary private publications
/// beneath this owned handle need no per-file inherited-ACL repair.
#[cfg(target_vendor = "apple")]
pub(super) fn private_directory_acl(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn acl_get_fd(fd: libc::c_int) -> *mut std::ffi::c_void;
        fn acl_get_entry(
            acl: *mut std::ffi::c_void,
            entry_id: libc::c_int,
            entry: *mut *mut std::ffi::c_void,
        ) -> libc::c_int;
        fn acl_free(acl: *mut std::ffi::c_void) -> libc::c_int;
    }
    // SAFETY: the descriptor is owned; the ACL is freed after the entry query.
    let acl = unsafe { acl_get_fd(file.as_raw_fd()) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let mut entry = std::ptr::null_mut();
    // Darwin returns zero for an entry and -1/EINVAL at the end of a valid ACL.
    // https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/acl_get_entry.3.html
    // SAFETY: acl is live and entry is a writable output slot.
    let found = unsafe { acl_get_entry(acl, 0, &raw mut entry) };
    let error = io::Error::last_os_error();
    // SAFETY: acl_get_fd returned this allocation; ownership was not transferred.
    unsafe {
        acl_free(acl);
    }
    if found == 0 {
        access_acl(None, file)
    } else if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOENT)) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(target_os = "linux")]
fn linux_xattrs(source: &File, content: u64, limit: usize) -> io::Result<Vec<u8>> {
    linux_xattrs_checked(source, content, limit, 0, &|_| false)
}

#[cfg(target_os = "linux")]
fn linux_xattrs_checked(
    source: &File,
    content: u64,
    limit: usize,
    copied: u64,
    should_cancel: &(dyn Fn(u64) -> bool + Sync),
) -> io::Result<Vec<u8>> {
    use rustix::fs::{fgetxattr, flistxattr};
    check_cancel(should_cancel, copied)?;
    let mut remaining = (limit as u64)
        .checked_sub(content)
        .ok_or(io::ErrorKind::FileTooLarge)?;
    let size = match flistxattr(source, &mut [0u8; 0][..]) {
        Ok(size) => size,
        Err(rustix::io::Errno::NOTSUP) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    remaining = remaining
        .checked_sub(size as u64)
        .ok_or(io::ErrorKind::FileTooLarge)?;
    let mut names = vec![0; size];
    let length = flistxattr(source, names.as_mut_slice())?;
    if length > names.len() {
        return Err(io::ErrorKind::ResourceBusy.into());
    }
    names.truncate(length);
    for name in names.split_inclusive(|b| *b == 0) {
        check_cancel(should_cancel, copied)?;
        let name =
            std::ffi::CStr::from_bytes_with_nul(name).map_err(|_| io::ErrorKind::InvalidData)?;
        let size = fgetxattr(source, name, &mut [0u8; 0][..])?;
        remaining = remaining
            .checked_sub(size as u64)
            .ok_or(io::ErrorKind::FileTooLarge)?;
    }
    Ok(names)
}

#[cfg(target_os = "linux")]
fn copy_xattrs(
    source: &File,
    destination: &File,
    content: usize,
    limit: usize,
    copied: &mut u64,
    should_cancel: &(dyn Fn(u64) -> bool + Sync),
) -> io::Result<()> {
    use rustix::fs::{XattrFlags, fgetxattr, fremovexattr, fsetxattr};
    let names = linux_xattrs_checked(source, content as u64, limit, *copied, should_cancel)?;
    let mut remaining = limit - content - names.len();
    // Strip ACLs inherited from a directory whose defaults changed since capture.
    for name in linux_xattrs_checked(destination, 0, limit, *copied, should_cancel)?
        .split_inclusive(|b| *b == 0)
    {
        check_cancel(should_cancel, *copied)?;
        if !names
            .split_inclusive(|b| *b == 0)
            .any(|source| source == name)
        {
            let name = std::ffi::CStr::from_bytes_with_nul(name)
                .map_err(|_| io::ErrorKind::InvalidData)?;
            fremovexattr(destination, name)?;
        }
    }
    for name in names.split_inclusive(|b| *b == 0) {
        check_cancel(should_cancel, *copied)?;
        let name =
            std::ffi::CStr::from_bytes_with_nul(name).map_err(|_| io::ErrorKind::InvalidData)?;
        let size = fgetxattr(source, name, &mut [0u8; 0][..])?;
        remaining = remaining
            .checked_sub(size)
            .ok_or(io::ErrorKind::FileTooLarge)?;
        let mut value = vec![0; size];
        let length = fgetxattr(source, name, value.as_mut_slice())?;
        if length != size {
            return Err(io::ErrorKind::ResourceBusy.into());
        }
        fsetxattr(destination, name, &value, XattrFlags::empty())?;
        *copied += size as u64 + name.to_bytes_with_nul().len() as u64;
        check_cancel(should_cancel, *copied)?;
    }
    Ok(())
}

#[cfg(unix)]
fn preserved_metadata_matches(source: &File, snapshot: &File, limit: usize) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let (before, copied) = (source.metadata()?, snapshot.metadata()?);
    if (before.mode(), before.uid(), before.gid()) != (copied.mode(), copied.uid(), copied.gid()) {
        return Ok(false);
    }
    #[cfg(target_vendor = "apple")]
    {
        use std::os::macos::fs::MetadataExt;
        if (
            before.st_flags(),
            before.st_birthtime(),
            before.st_birthtime_nsec(),
        ) != (
            copied.st_flags(),
            copied.st_birthtime(),
            copied.st_birthtime_nsec(),
        ) || mac_acl(source)? != mac_acl(snapshot)?
        {
            return Ok(false);
        }
    }
    #[cfg(target_vendor = "apple")]
    let names = |file: &File| mac_xattrs(file, 0, limit);
    #[cfg(target_os = "linux")]
    let names = |file: &File| linux_xattrs(file, 0, limit);
    let (before, copied) = (names(source)?, names(snapshot)?);
    // Equal counts plus a successful lookup of every source name establish equal
    // name sets without allocating another copy or a per-attribute search index.
    if before.split_inclusive(|b| *b == 0).count() != copied.split_inclusive(|b| *b == 0).count() {
        return Ok(false);
    }
    let mut remaining = limit - before.len();
    for name in before.split_inclusive(|b| *b == 0) {
        let name =
            std::ffi::CStr::from_bytes_with_nul(name).map_err(|_| io::ErrorKind::InvalidData)?;
        let size = rustix::fs::fgetxattr(source, name, &mut [0u8; 0][..])?;
        remaining = remaining
            .checked_sub(size)
            .ok_or(io::ErrorKind::FileTooLarge)?;
        if rustix::fs::fgetxattr(snapshot, name, &mut [0u8; 0][..])? != size {
            return Ok(false);
        }
        #[cfg(target_os = "linux")]
        {
            let mut a = vec![0; size];
            let mut b = vec![0; size];
            if rustix::fs::fgetxattr(source, name, a.as_mut_slice())? != size
                || rustix::fs::fgetxattr(snapshot, name, b.as_mut_slice())? != size
                || a != b
            {
                return Ok(false);
            }
        }
        #[cfg(target_vendor = "apple")]
        {
            use std::os::fd::AsRawFd;
            let fork = name == c"com.apple.ResourceFork";
            let mut a = vec![0u8; if fork { size.min(65536) } else { size }];
            let mut b = vec![0u8; a.len()];
            let mut at = 0;
            loop {
                let count = (size - at).min(a.len());
                let position = u32::try_from(at).map_err(|_| io::ErrorKind::FileTooLarge)?;
                for (file, buffer) in [(source, &mut a), (snapshot, &mut b)] {
                    // SAFETY: the terminated name and owned file are live, and count fits buffer.
                    let read = mac_count(unsafe {
                        libc::fgetxattr(
                            file.as_raw_fd(),
                            name.as_ptr(),
                            buffer.as_mut_ptr().cast(),
                            count,
                            position,
                            0,
                        )
                    })?;
                    if read != count {
                        return Ok(false);
                    }
                }
                if a[..count] != b[..count] {
                    return Ok(false);
                }
                at += count;
                if at == size {
                    break;
                }
            }
        }
    }
    Ok(true)
}

#[cfg(target_vendor = "apple")]
fn mac_acl(file: &File) -> io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn acl_get_fd(fd: libc::c_int) -> *mut std::ffi::c_void;
        fn acl_size(acl: *mut std::ffi::c_void) -> libc::ssize_t;
        fn acl_copy_ext(
            buffer: *mut std::ffi::c_void,
            acl: *mut std::ffi::c_void,
            size: libc::ssize_t,
        ) -> libc::ssize_t;
        fn acl_free(acl: *mut std::ffi::c_void) -> libc::c_int;
    }
    // SAFETY: file owns its descriptor; acl is freed after serialization.
    let acl = unsafe { acl_get_fd(file.as_raw_fd()) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(Vec::new())
        } else {
            Err(error)
        };
    }
    let result = (|| {
        // SAFETY: acl remains live until acl_free below.
        let size = mac_count(unsafe { acl_size(acl) })?;
        // Darwin limits ACLs to 128 entries; cap native serialization defensively.
        if size > 65536 {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let mut bytes = vec![0u8; size];
        // SAFETY: bytes owns size writable bytes and acl is a valid live object.
        let copied =
            mac_count(unsafe { acl_copy_ext(bytes.as_mut_ptr().cast(), acl, size.cast_signed()) })?;
        bytes.truncate(copied);
        Ok(bytes)
    })();
    // SAFETY: acl_get_fd allocated this object and no call transferred its ownership.
    unsafe {
        acl_free(acl);
    }
    result
}

#[cfg(windows)]
fn preserved_metadata_matches(source: &File, snapshot: &File, limit: usize) -> io::Result<bool> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_ARCHIVE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    };
    let (a, b) = (source.metadata()?, snapshot.metadata()?);
    if a.creation_time() != b.creation_time()
        || a.file_attributes() & !FILE_ATTRIBUTE_ARCHIVE
            != b.file_attributes() & !FILE_ATTRIBUTE_ARCHIVE
        || !windows_policy_matches(source, snapshot)?
    {
        return Ok(false);
    }
    let mut before = stream_inventory(source, limit)?;
    before.retain(|(name, _)| name != "::$DATA");
    let mut copied = stream_inventory(snapshot, limit)?;
    copied.retain(|(name, _)| name != "::$DATA");
    before.sort_unstable();
    copied.sort_unstable();
    if before != copied {
        return Ok(false);
    }
    let open = |base: &File, name: &std::ffi::OsStr| -> io::Result<File> {
        let mut path = super::final_path(base)?.into_os_string();
        path.push(name);
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(path)?;
        if !same_object(base, &file)? {
            return Err(io::ErrorKind::ResourceBusy.into());
        }
        Ok(file)
    };
    let mut a = [0; 8192];
    let mut b = [0; 8192];
    for (name, size) in before {
        let (mut left, mut right) = (open(source, &name)?, open(snapshot, &name)?);
        let mut remaining = size;
        while remaining > 0 {
            let count =
                usize::try_from(remaining.min(a.len() as u64)).expect("bounded stream chunk");
            left.read_exact(&mut a[..count])?;
            right.read_exact(&mut b[..count])?;
            if a[..count] != b[..count] {
                return Ok(false);
            }
            remaining -= count as u64;
        }
        if left.read(&mut a[..1])? != 0 || right.read(&mut b[..1])? != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(windows)]
fn stream_inventory(file: &File, limit: usize) -> io::Result<Vec<(OsString, u64)>> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_STREAM_INFO, FileStreamInfo, GetFileInformationByHandleEx,
    };
    let mut buffer = vec![0u64; 65536 / 8];
    // SAFETY: buffer is initialized, aligned storage with the advertised byte capacity.
    crate::windows::ok(unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStreamInfo,
            buffer.as_mut_ptr().cast(),
            65536,
        )
    })?;
    let mut at = 0usize;
    let mut total = 0u64;
    let mut streams = Vec::new();
    loop {
        if at + size_of::<FILE_STREAM_INFO>() > 65536 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let header = buffer
            .as_ptr()
            .wrapping_add(at / 8)
            .cast::<FILE_STREAM_INFO>();
        // SAFETY: at is aligned and bounds cover the kernel-populated header.
        let entry = unsafe { &*header };
        let length = entry.StreamNameLength as usize;
        if length % 2 != 0
            || at + std::mem::offset_of!(FILE_STREAM_INFO, StreamName) + length > 65536
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the checked length covers exactly this UTF-16 name in the live
        // buffer; the pointer derives from the whole buffer, not the 1-element field.
        let name = OsString::from_wide(unsafe {
            std::slice::from_raw_parts((&raw const (*header).StreamName).cast::<u16>(), length / 2)
        });
        let size = u64::try_from(entry.StreamSize).map_err(|_| io::ErrorKind::InvalidData)?;
        {
            total = total
                .checked_add(size)
                .filter(|sum| *sum <= limit as u64)
                .ok_or(io::ErrorKind::FileTooLarge)?;
            streams.push((name, size));
        }
        if entry.NextEntryOffset == 0 {
            break;
        }
        let next = entry.NextEntryOffset as usize;
        if next < size_of::<FILE_STREAM_INFO>() || next % 8 != 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        at = at.checked_add(next).ok_or(io::ErrorKind::InvalidData)?;
    }
    Ok(streams)
}

#[cfg(windows)]
struct WindowsPolicy(Vec<u64>);

#[cfg(windows)]
impl WindowsPolicy {
    const INFORMATION: u32 = windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION
        | windows_sys::Win32::Security::GROUP_SECURITY_INFORMATION
        | windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    fn read(file: &File) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Wdk::Storage::FileSystem::NtQuerySecurityObject;
        use windows_sys::Win32::Security::{SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE};
        let mut needed = 0;
        // SAFETY: a null buffer queries the size of the owned object's descriptor.
        let status = unsafe {
            NtQuerySecurityObject(
                file.as_raw_handle(),
                Self::INFORMATION,
                std::ptr::null_mut(),
                0,
                &raw mut needed,
            )
        };
        let maximum = usize::from(u16::MAX)
            + 2 * SECURITY_MAX_SID_SIZE as usize
            + size_of::<SECURITY_DESCRIPTOR>();
        if needed == 0 {
            // SAFETY: this converts the returned scalar status without dereferencing pointers.
            return Err(io::Error::from_raw_os_error(
                unsafe { windows_sys::Win32::Foundation::RtlNtStatusToDosError(status) }
                    .cast_signed(),
            ));
        }
        if needed as usize > maximum {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        let mut policy = Self(vec![0; (needed as usize).div_ceil(8)]);
        // SAFETY: aligned initialized storage covers needed writable bytes.
        let status = unsafe {
            NtQuerySecurityObject(
                file.as_raw_handle(),
                Self::INFORMATION,
                policy.0.as_mut_ptr().cast(),
                needed,
                &raw mut needed,
            )
        };
        if status < 0 {
            // SAFETY: converts the scalar native status.
            return Err(io::Error::from_raw_os_error(
                unsafe { windows_sys::Win32::Foundation::RtlNtStatusToDosError(status) }
                    .cast_signed(),
            ));
        }
        Ok(policy)
    }

    fn descriptor(&self) -> windows_sys::Win32::Security::PSECURITY_DESCRIPTOR {
        self.0.as_ptr().cast_mut().cast()
    }

    fn matches(&self, other: &Self) -> io::Result<bool> {
        use windows_sys::Win32::Security::{
            EqualSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
            GetSecurityDescriptorGroup, GetSecurityDescriptorOwner, SE_DACL_AUTO_INHERIT_REQ,
            SE_DACL_AUTO_INHERITED, SE_DACL_DEFAULTED, SE_DACL_PRESENT, SE_DACL_PROTECTED,
            SE_GROUP_DEFAULTED, SE_OWNER_DEFAULTED,
        };
        let parts = |policy: &Self| -> io::Result<_> {
            let (mut owner, mut group, mut acl) = (
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            let (mut defaulted, mut present, mut control, mut revision) = (0, 0, 0, 0);
            // SAFETY: policy retains the kernel-validated self-relative descriptor;
            // all output pointers are writable for their documented types.
            unsafe {
                crate::windows::ok(GetSecurityDescriptorOwner(
                    policy.descriptor(),
                    &raw mut owner,
                    &raw mut defaulted,
                ))?;
                crate::windows::ok(GetSecurityDescriptorGroup(
                    policy.descriptor(),
                    &raw mut group,
                    &raw mut defaulted,
                ))?;
                crate::windows::ok(GetSecurityDescriptorDacl(
                    policy.descriptor(),
                    &raw mut present,
                    &raw mut acl,
                    &raw mut defaulted,
                ))?;
                crate::windows::ok(GetSecurityDescriptorControl(
                    policy.descriptor(),
                    &raw mut control,
                    &raw mut revision,
                ))?;
            }
            Ok((owner, group, acl, control))
        };
        let (a, b) = (parts(self)?, parts(other)?);
        let controls = SE_OWNER_DEFAULTED
            | SE_GROUP_DEFAULTED
            | SE_DACL_PRESENT
            | SE_DACL_DEFAULTED
            | SE_DACL_AUTO_INHERIT_REQ
            | SE_DACL_AUTO_INHERITED
            | SE_DACL_PROTECTED;
        // SAFETY: descriptors retain all SID/ACL pointers; null SIDs compare only as null.
        unsafe {
            let sid = |a: *mut std::ffi::c_void, b: *mut std::ffi::c_void| {
                if a.is_null() || b.is_null() {
                    a == b
                } else {
                    EqualSid(a, b) != 0
                }
            };
            Ok(a.3 & controls == b.3 & controls
                && sid(a.0, b.0)
                && sid(a.1, b.1)
                && same_acl(a.2, b.2)?)
        }
    }
}

#[cfg(windows)]
fn windows_policy_matches(source: &File, snapshot: &File) -> io::Result<bool> {
    WindowsPolicy::read(source)?.matches(&WindowsPolicy::read(snapshot)?)
}

#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq)]
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

/// The volume and file id of `file`: what names it whatever its path.
#[cfg(windows)]
pub(super) fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };
    let mut identity = FILE_ID_INFO::default();
    // SAFETY: the output buffer has its advertised type and size; file owns the handle.
    unsafe {
        crate::windows::ok(GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut identity).cast(),
            u32::try_from(size_of::<FILE_ID_INFO>()).expect("fixed structure size"),
        ))?;
    }
    Ok((identity.VolumeSerialNumber, identity.FileId.Identifier))
}

#[cfg(windows)]
fn signature(file: &File) -> io::Result<Signature> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
    };
    let (volume, identity) = file_id(file)?;
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: the output buffer has its advertised type and size; file owns the handle.
    unsafe {
        crate::windows::ok(GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&raw mut basic).cast(),
            u32::try_from(size_of::<FILE_BASIC_INFO>()).expect("fixed structure size"),
        ))?;
    }
    Ok(Signature {
        volume,
        identity,
        size: file.metadata()?.len(),
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
        attributes: basic.FileAttributes,
    })
}

#[cfg(windows)]
fn stage_windows(
    transaction: &EditTransaction,
    directory: &Directory<'_>,
    should_cancel: &(dyn Fn(u64) -> bool + Sync),
) -> io::Result<File> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        COPY_FILE_COPY_SYMLINK, COPY_FILE_FAIL_IF_EXISTS, CopyFileExW, DELETE, WRITE_DAC,
        WRITE_OWNER,
    };
    let access = GENERIC_READ | GENERIC_WRITE | DELETE | WRITE_DAC | WRITE_OWNER;
    let Some(CapturedSource { file: source, .. }) = &transaction.source else {
        return super::staging::relative(
            &directory.file,
            std::ffi::OsStr::new(super::staging::CONTENT),
            ObjectKind::File,
            Creation::CreateNew,
        );
    };
    stream_bytes(source, transaction.limit.unwrap_or(usize::MAX))?;
    let context = std::sync::Mutex::new(CopyContext {
        source,
        destination: None,
        error: None,
        should_cancel,
        limit: transaction.limit.map_or(u64::MAX, |limit| limit as u64),
        announced: 0,
        last_stream: 0,
    });
    let from = crate::windows::wide(transaction.directory.join(&transaction.name).as_os_str());
    let to = crate::windows::wide(directory.path.join(super::staging::CONTENT).as_os_str());
    // The pinned directories deny delete sharing. CopyFileEx's path is therefore
    // inside our private namespace for the entire synchronous native operation.
    // The callback restricts access before copying and checks every work chunk.
    // SAFETY: paths and synchronized callback context live until the copy returns.
    let copied = crate::windows::ok(unsafe {
        CopyFileExW(
            from.as_ptr(),
            to.as_ptr(),
            Some(copy_progress),
            (&raw const context).cast(),
            std::ptr::null_mut(),
            COPY_FILE_FAIL_IF_EXISTS | COPY_FILE_COPY_SYMLINK,
        )
    });
    let mut context = context
        .into_inner()
        .map_err(|_| io::Error::other("native copy callback panicked"))?;
    if let Err(error) = copied {
        if let Some(keeper) = &context.destination
            && let Ok(file) = reopen(keeper.as_raw_handle(), DELETE)
        {
            super::staging::delete(&file);
        }
        return Err(context.error.take().unwrap_or(error));
    }
    let keeper = context
        .destination
        .ok_or_else(|| io::Error::other("native copy omitted its stream callback"))?;
    // Upgrade the same object after the copier releases its data-sharing lease.
    match reopen_shared(
        keeper.as_raw_handle(),
        access,
        windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
            | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE,
    ) {
        Ok(file) => Ok(file),
        Err(error) => {
            if let Ok(file) = reopen(keeper.as_raw_handle(), DELETE) {
                super::staging::delete(&file);
            }
            Err(error)
        }
    }
}

#[cfg(windows)]
struct CopyContext<'a> {
    source: &'a File,
    destination: Option<File>,
    error: Option<io::Error>,
    should_cancel: &'a (dyn Fn(u64) -> bool + Sync),
    limit: u64,
    announced: u64,
    last_stream: u32,
}

#[cfg(windows)]
fn stream_bytes(file: &File, limit: usize) -> io::Result<u64> {
    Ok(stream_inventory(file, limit)?
        .iter()
        .map(|(_, size)| size)
        .sum())
}

#[cfg(windows)]
pub(super) fn reopen(
    handle: windows_sys::Win32::Foundation::HANDLE,
    access: u32,
) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    reopen_shared(
        handle,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )
}

#[cfg(windows)]
fn reopen_shared(
    handle: windows_sys::Win32::Foundation::HANDLE,
    access: u32,
    sharing: u32,
) -> io::Result<File> {
    // SAFETY: callers retain the borrowed handle; the new handle is checked and owned.
    Ok(File::from(crate::windows::owned(unsafe {
        windows_sys::Win32::Storage::FileSystem::ReOpenFile(handle, access, sharing, 0)
    })?))
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn copy_progress(
    total_size: i64,
    copied: i64,
    stream_size: i64,
    _: i64,
    stream: u32,
    reason: u32,
    source: windows_sys::Win32::Foundation::HANDLE,
    destination: windows_sys::Win32::Foundation::HANDLE,
    data: *const std::ffi::c_void,
) -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{
        CALLBACK_STREAM_SWITCH, FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES, PROGRESS_CANCEL,
        PROGRESS_CONTINUE, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    };
    // SAFETY: context lives until CopyFileEx returns. A mutex also permits
    // callback delivery from native workers without aliasing mutable state.
    let context = unsafe { &*data.cast::<std::sync::Mutex<CopyContext<'_>>>() };
    let Ok(mut context) = context.lock() else {
        return PROGRESS_CANCEL;
    };
    if context.destination.is_none() {
        let result = (|| {
            let copied_source = reopen(source, FILE_READ_ATTRIBUTES)?;
            if !same_object(context.source, &copied_source)? {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "edit source changed during native copy",
                ));
            }
            let keeper = reopen(
                destination,
                FILE_READ_ATTRIBUTES
                    | FILE_WRITE_ATTRIBUTES
                    | READ_CONTROL
                    | WRITE_DAC
                    | WRITE_OWNER,
            )?;
            copy_access_policy(context.source, &keeper)?;
            context.destination = Some(keeper);
            Ok(())
        })();
        if let Err(error) = result {
            context.error = Some(error);
            return PROGRESS_CANCEL;
        }
    }
    let checked = (|| {
        let size = u64::try_from(total_size).map_err(|_| io::ErrorKind::InvalidData)?;
        let copied = u64::try_from(copied).map_err(|_| io::ErrorKind::InvalidData)?;
        if size > context.limit || copied > context.limit {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        if reason == CALLBACK_STREAM_SWITCH && stream != context.last_stream {
            let bytes = u64::try_from(stream_size).map_err(|_| io::ErrorKind::InvalidData)?;
            context.announced = context
                .announced
                .checked_add(bytes)
                .filter(|sum| *sum <= context.limit)
                .ok_or(io::ErrorKind::FileTooLarge)?;
            context.last_stream = stream;
        }
        check_cancel(context.should_cancel, copied)
    })();
    if let Err(error) = checked {
        context.error = Some(error);
        return PROGRESS_CANCEL;
    }
    PROGRESS_CONTINUE
}

#[cfg(windows)]
fn preserve_metadata(source: &File, destination: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
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
    copy_access_policy(source, destination)
}

#[cfg(windows)]
fn copy_access_policy(source: &File, destination: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    // Compare the stored descriptor, without parent-dependent ACE presentation flags.
    let policy = WindowsPolicy::read(source)?;
    // SAFETY: both the destination handle and aligned descriptor remain live.
    let status = unsafe {
        windows_sys::Wdk::Storage::FileSystem::NtSetSecurityObject(
            destination.as_raw_handle(),
            WindowsPolicy::INFORMATION,
            policy.descriptor(),
        )
    };
    if status < 0 {
        // SAFETY: converts the returned scalar status.
        return Err(io::Error::from_raw_os_error(
            unsafe { windows_sys::Win32::Foundation::RtlNtStatusToDosError(status) }.cast_signed(),
        ));
    }
    if !policy.matches(&WindowsPolicy::read(destination)?)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot preserve the source access policy",
        ));
    }
    Ok(())
}

#[cfg(windows)]
unsafe fn same_acl(
    expected: *const windows_sys::Win32::Security::ACL,
    actual: *const windows_sys::Win32::Security::ACL,
) -> io::Result<bool> {
    use windows_sys::Win32::Security::{
        ACL, ACL_SIZE_INFORMATION, AclSizeInformation, GetAclInformation,
    };
    if expected.is_null() || actual.is_null() {
        return Ok(expected == actual);
    }
    let mut before = ACL_SIZE_INFORMATION::default();
    let mut after = ACL_SIZE_INFORMATION::default();
    let size = u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).expect("fixed structure size");
    // SAFETY: callers retain the descriptors containing both kernel-validated ACLs.
    unsafe {
        crate::windows::ok(GetAclInformation(
            expected,
            (&raw mut before).cast(),
            size,
            AclSizeInformation,
        ))?;
        crate::windows::ok(GetAclInformation(
            actual,
            (&raw mut after).cast(),
            size,
            AclSizeInformation,
        ))?;
        if before.AceCount != after.AceCount
            || before.AclBytesInUse != after.AclBytesInUse
            || (*expected).AclRevision != (*actual).AclRevision
        {
            return Ok(false);
        }
        let length =
            usize::try_from(before.AclBytesInUse).map_err(|_| io::ErrorKind::InvalidData)?;
        let header = size_of::<ACL>();
        if length < header
            || length > usize::from((*expected).AclSize)
            || length > usize::from((*actual).AclSize)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // Allocation slack is not access policy. Compare the ACE region only.
        Ok(
            std::slice::from_raw_parts(expected.cast::<u8>().add(header), length - header)
                == std::slice::from_raw_parts(actual.cast::<u8>().add(header), length - header),
        )
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[cfg(unix)]
    fn assert_changed(result: &Result<EditReceipt, EditError>) {
        assert!(
            matches!(result, Err(EditError::Conflict(EditConflict::Changed))),
            "{result:?}"
        );
    }

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
            EditTransaction::capture(&self.target(), Some(1 << 20)).unwrap()
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
        let receipt = work
            .capture()
            .publish(b"replacement", Persistence::Visible, &|_| false)
            .unwrap();
        assert!(receipt.visible);
        assert_eq!(receipt.durability, Durability::NotRequested);
        assert!(matches!(
            receipt.protection,
            PublicationProtection::ExchangeVerified
                | PublicationProtection::RevalidatedAtomicReplace
        ));
        let mut old = String::new();
        reader.read_to_string(&mut old).unwrap();
        assert_eq!(old, "original");
        assert_eq!(fs::read(work.target()).unwrap(), b"replacement");
        work.only_target_remains();
    }

    #[test]
    fn empty_files_and_shorter_replacements_have_exact_contents() {
        let work = Workspace::new();
        fs::write(work.target(), b"").unwrap();
        for content in [b"longer content".as_slice(), b"x", b""] {
            work.capture()
                .publish(content, Persistence::Visible, &|_| false)
                .unwrap();
            assert_eq!(fs::read(work.target()).unwrap(), content);
        }
        work.only_target_remains();
    }

    #[cfg(unix)]
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
            transaction.publish(b"ours", Persistence::Visible, &|_| false),
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
        fs::remove_file(work.target()).unwrap();
        fs::write(work.target(), b"original").unwrap();
        assert!(matches!(
            transaction.publish(b"ours", Persistence::Visible, &|_| false),
            Err(EditError::Conflict(EditConflict::Changed))
        ));
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
    }

    #[test]
    fn competing_edits_of_one_version_never_both_commit() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let first = work.capture();
        let second = work.capture();
        first
            .publish(b"first", Persistence::Visible, &|_| false)
            .unwrap();
        assert!(matches!(
            second.publish(b"second", Persistence::Visible, &|_| false),
            Err(EditError::Conflict(EditConflict::Changed))
        ));
        assert_eq!(fs::read(work.target()).unwrap(), b"first");
        work.only_target_remains();
    }

    #[test]
    fn competing_creators_never_overwrite_each_other() {
        let work = Workspace::new();
        let first = work.capture();
        let second = work.capture();
        assert!(first.original().is_none());
        first
            .publish(b"first", Persistence::Visible, &|_| false)
            .unwrap();
        assert!(matches!(
            second.publish(b"second", Persistence::Visible, &|_| false),
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
            transaction.publish(b"ours", Persistence::Visible, &|_| false),
            Err(EditError::Conflict(EditConflict::Changed))
        ));
        assert_eq!(fs::read_dir(&work.0).unwrap().count(), 0);
    }

    #[test]
    fn a_truncated_output_stream_never_publishes() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let mut output = tempfile::tempfile().unwrap();
        output.write_all(b"short").unwrap();
        output.rewind().unwrap();
        assert!(
            work.capture()
                .publish_reader(&mut output, 6, Persistence::Visible, &|_| false)
                .is_err()
        );
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
    }

    #[test]
    fn captured_bytes_stay_original_when_the_source_changes() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let transaction = work.capture();
        #[cfg(unix)]
        fs::write(work.target(), b"changed").unwrap();
        #[cfg(windows)]
        {
            assert_eq!(
                fs::write(work.target(), b"changed")
                    .unwrap_err()
                    .raw_os_error(),
                Some(32)
            );
            fs::remove_file(work.target()).unwrap();
            fs::write(work.target(), b"changed").unwrap();
        }
        let mut captured = Vec::new();
        transaction.original().unwrap().rewind().unwrap();
        transaction
            .original()
            .unwrap()
            .read_to_end(&mut captured)
            .unwrap();
        assert_eq!(captured, b"original");
        assert!(matches!(
            transaction.publish(b"ours", Persistence::Visible, &|_| false),
            Err(EditError::Conflict(EditConflict::Changed))
        ));
        assert_eq!(fs::read(work.target()).unwrap(), b"changed");
        work.only_target_remains();
    }

    #[test]
    fn rejection_and_abandonment_leave_the_original_untouched() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        assert!(matches!(
            EditTransaction::capture(&work.target(), Some(2)),
            Err(EditError::TooLarge)
        ));
        drop(work.capture());
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
        assert!(matches!(
            EditTransaction::capture(&work.0, Some(16)),
            Err(EditError::NotRegular)
        ));
    }

    #[test]
    fn abandoning_a_partial_staging_write_reclaims_only_the_temporary_file() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let transaction = work.capture();
        let mut staged = transaction.stage(&|_| false).unwrap();
        staged.file.write_all(b"partial").unwrap();
        drop(staged);
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn staged_content_never_inherits_public_read_access() {
        use std::os::unix::fs::PermissionsExt;
        let work = Workspace(Path::new("/private/tmp").join(temporary_name()));
        fs::create_dir(&work.0).unwrap();
        fs::set_permissions(&work.0, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(work.target(), b"original").unwrap();
        let reader = |path: &Path| {
            std::process::Command::new("/usr/bin/sudo")
                .args(["-n", "-u", "nobody", "/bin/cat"])
                .arg(path)
                .output()
                .unwrap()
        };
        let native_reader = std::env::var_os("GITHUB_ACTIONS").is_some();
        if native_reader {
            let control = work.0.join("public");
            fs::write(&control, b"control").unwrap();
            fs::set_permissions(&control, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(
                reader(&control).stdout,
                b"control",
                "the unprivileged reader must reach the fixture"
            );
        }
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone allow read,file_inherit,directory_inherit"])
                .arg(&work.0)
                .status()
                .unwrap()
                .success()
        );
        let transaction = EditTransaction::capture(&work.target(), Some(8 << 20)).unwrap();
        let mut staged = transaction.stage(&|_| false).unwrap();
        staged.file.write_all(&vec![b'x'; 8 << 20]).unwrap();
        for path in [
            work.0.join(&staged.directory.name),
            work.0
                .join(&staged.directory.name)
                .join(super::super::staging::CONTENT),
        ] {
            let output = std::process::Command::new("/bin/ls")
                .arg("-led")
                .arg(path)
                .output()
                .unwrap();
            assert!(output.status.success());
            let acl = String::from_utf8(output.stdout).unwrap();
            assert!(
                !acl.contains("allow read"),
                "staged data grants public read: {acl}"
            );
        }
        if native_reader {
            let path = work
                .0
                .join(&staged.directory.name)
                .join(super::super::staging::CONTENT);
            let denied = reader(&path);
            assert!(!denied.status.success());
            assert!(
                denied.stdout.is_empty(),
                "no staged byte may reach the reader"
            );
        }
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn staging_substitution_never_publishes_an_unrelated_object() {
        use std::os::unix::fs::symlink;
        for (fallback, replacing) in [false, true]
            .into_iter()
            .flat_map(|fallback| [false, true].map(move |replacing| (fallback, replacing)))
        {
            let work = Workspace::new();
            if replacing {
                fs::write(work.target(), b"original").unwrap();
            }
            let victim = work.0.join("victim");
            fs::write(&victim, b"unrelated").unwrap();
            let transaction = work.capture();
            let mut staged = transaction.stage(&|_| false).unwrap();
            let public = work.0.join(&staged.directory.name);
            fs::rename(&public, work.0.join("displaced")).unwrap();
            symlink(&victim, &public).unwrap();
            staged.file.write_all(b"replacement").unwrap();
            transaction.revalidate().unwrap();
            if replacing {
                preserve_metadata(&transaction.source.as_ref().unwrap().file, &staged.file)
                    .unwrap();
                transaction
                    .commit(&mut staged, !fallback, || Ok(()), || {}, || {})
                    .unwrap();
            } else if fallback {
                staged
                    .publish_without_native_rename(&transaction.name)
                    .unwrap();
            } else {
                staged.publish(&transaction.name).unwrap();
            }
            drop(staged);
            assert!(
                fs::symlink_metadata(&public)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "cleanup must preserve the substitute"
            );
            assert_eq!(fs::read_dir(work.0.join("displaced")).unwrap().count(), 0);
            assert!(
                !fs::symlink_metadata(work.target())
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read(work.target()).unwrap(), b"replacement");
            assert_eq!(fs::read(&victim).unwrap(), b"unrelated");
        }
    }

    #[test]
    fn cancellation_after_staging_never_changes_the_target() {
        let work = Workspace::new();
        fs::write(work.target(), b"").unwrap();
        let result = work
            .capture()
            .publish(b"replacement", Persistence::Visible, &|copied| copied >= 11);
        assert!(matches!(result, Err(EditError::Cancelled)));
        assert_eq!(fs::read(work.target()).unwrap(), b"");
        work.only_target_remains();
    }

    #[cfg(unix)]
    #[test]
    fn reconstructible_saves_do_not_require_a_directory_barrier() {
        use std::os::unix::fs::PermissionsExt;
        let work = Workspace::new();
        fs::set_permissions(&work.0, fs::Permissions::from_mode(0o300)).unwrap();
        let denied = File::open(&work.0).is_err();
        let saved = super::super::write_private(&work.target(), b"discovery");
        fs::set_permissions(&work.0, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(denied, "the fixture must deny a directory flush handle");
        saved.unwrap();
        assert_eq!(fs::read(work.target()).unwrap(), b"discovery");
    }

    #[test]
    fn hard_links_are_never_silently_severed() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let alias = work.0.join("alias");
        fs::hard_link(work.target(), &alias).unwrap();
        assert!(matches!(
            EditTransaction::capture(&work.target(), Some(16)),
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
                EditTransaction::capture(path, Some(32)),
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
        work.capture()
            .publish(b"new", Persistence::Visible, &|_| false)
            .unwrap();
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
            .publish_with_sync(
                b"new",
                Persistence::Durable,
                |_| Err(io::Error::other("injected directory flush failure")),
                &|_| false,
                true,
                || {},
            )
            .unwrap();
        assert!(receipt.visible);
        assert!(matches!(receipt.durability, Durability::Failed(_)));
        assert_eq!(fs::read(work.target()).unwrap(), b"new");
        let receipt = work
            .capture()
            .publish(b"durable", Persistence::Durable, &|_| false)
            .unwrap();
        assert_eq!(receipt.durability, Durability::Confirmed);
        work.only_target_remains();
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_parent_is_a_conflict_without_redirecting_the_write() {
        let work = Workspace::new();
        let parent = work.0.join("parent");
        let moved = work.0.join("moved");
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("file"), b"original").unwrap();
        let transaction = EditTransaction::capture(&parent.join("file"), Some(4096)).unwrap();
        fs::rename(&parent, &moved).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("file"), b"unrelated").unwrap();
        assert!(matches!(
            transaction.publish(b"edited", Persistence::Visible, &|_| false),
            Err(EditError::Conflict(EditConflict::Changed))
        ));
        assert_eq!(fs::read(parent.join("file")).unwrap(), b"unrelated");
        assert_eq!(fs::read(moved.join("file")).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn redirecting_a_parent_alias_is_a_conflict() {
        use std::os::unix::fs::symlink;
        let work = Workspace::new();
        let first = work.0.join("first");
        let second = work.0.join("second");
        let alias = work.0.join("alias");
        for parent in [&first, &second] {
            fs::create_dir(parent).unwrap();
            fs::write(parent.join("file"), b"original").unwrap();
        }
        symlink(&first, &alias).unwrap();
        let transaction = EditTransaction::capture(&alias.join("file"), Some(4096)).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&second, &alias).unwrap();
        assert!(matches!(
            transaction.publish(b"edited", Persistence::Visible, &|_| false),
            Err(EditError::Conflict(EditConflict::Changed))
        ));
        for parent in [&first, &second] {
            assert_eq!(fs::read(parent.join("file")).unwrap(), b"original");
        }
    }

    #[cfg(unix)]
    #[test]
    fn fallback_publication_is_exclusive_even_after_the_last_check() {
        let work = Workspace::new();
        let transaction = work.capture();
        let mut staged = transaction.stage(&|_| false).unwrap();
        staged.file.write_all(b"ours").unwrap();
        transaction.revalidate().unwrap();
        fs::write(work.target(), b"theirs").unwrap();
        assert_eq!(
            staged
                .publish_without_native_rename(&transaction.name)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(work.target()).unwrap(), b"theirs");
        drop(staged);
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
        work.capture()
            .publish(b"new", Persistence::Visible, &|_| false)
            .unwrap();
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
        work.capture()
            .publish(b"new", Persistence::Visible, &|_| false)
            .unwrap();
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
        work.capture()
            .publish(b"new", Persistence::Visible, &|_| false)
            .unwrap();
        assert_eq!(entries(), before);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn safe_save_does_not_add_newly_inherited_macos_access() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow execute,file_inherit"])
            .arg(&work.0)
            .status()
            .unwrap();
        assert!(status.success());
        work.capture()
            .publish(b"new", Persistence::Visible, &|_| false)
            .unwrap();
        let output = std::process::Command::new("/bin/ls")
            .arg("-le")
            .arg(work.target())
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("allow execute"), "{text}");
        assert_eq!(fs::read(work.target()).unwrap(), b"new");
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
        work.capture()
            .publish(b"new", Persistence::Visible, &|_| false)
            .unwrap();
        assert_eq!(entries(), before);
    }

    #[cfg(windows)]
    #[test]
    fn moved_windows_files_do_not_gain_the_new_directorys_access() {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
        };
        let directory_policy = |path: &Path, sddl: &str| {
            fs::create_dir_all(path).unwrap();
            let text = crate::windows::wide(std::ffi::OsStr::new(sddl));
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: text is terminated and descriptor receives an owned allocation.
            crate::windows::ok(unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    SDDL_REVISION_1,
                    &raw mut descriptor,
                    std::ptr::null_mut(),
                )
            })
            .unwrap();
            let path = crate::windows::wide(path.as_os_str());
            // SAFETY: path and descriptor remain live through the call.
            let result = crate::windows::ok(unsafe {
                SetFileSecurityW(
                    path.as_ptr(),
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    descriptor,
                )
            });
            // SAFETY: the SDDL conversion allocated descriptor with LocalAlloc.
            unsafe {
                LocalFree(descriptor);
            }
            result.unwrap();
        };
        let work = Workspace::new();
        let sid = crate::windows::user_sid(None).unwrap();
        directory_policy(
            &work.0,
            &format!("D:P(A;OICI;FA;;;{sid})(A;OICI;GRGX;;;WD)"),
        );
        let private = work.0.join("private");
        directory_policy(&private, &format!("D:P(A;OICI;FA;;;{sid})"));
        fs::write(private.join("file"), b"original").unwrap();
        fs::rename(private.join("file"), work.target()).unwrap();
        let entries = || {
            let output = std::process::Command::new("icacls.exe")
                .arg(work.target())
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        let before = entries();
        assert!(!before.contains("Everyone"), "{before}");
        work.capture()
            .publish(b"new", Persistence::Visible, &|_| false)
            .unwrap();
        assert_eq!(entries(), before);
        assert_eq!(fs::read(work.target()).unwrap(), b"new");
    }

    #[cfg(windows)]
    #[test]
    fn alternate_streams_survive_while_stream_targets_are_rejected() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let stream = work.0.join("target:Zone.Identifier");
        fs::write(&stream, b"[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();
        assert!(matches!(
            EditTransaction::capture(&stream, Some(128)),
            Err(EditError::NotRegular)
        ));
        work.capture()
            .publish(b"new", Persistence::Visible, &|_| false)
            .unwrap();
        assert_eq!(fs::read(stream).unwrap(), b"[ZoneTransfer]\r\nZoneId=3\r\n");
        work.only_target_remains();
    }

    #[cfg(windows)]
    #[test]
    fn alternate_stream_bytes_are_part_of_the_edit_limit() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        fs::write(work.0.join("target:payload"), vec![b'x'; 65536]).unwrap();
        assert!(matches!(
            EditTransaction::capture(&work.target(), Some(128)),
            Err(EditError::TooLarge)
        ));
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
    }

    #[cfg(windows)]
    #[test]
    fn cancellation_during_an_alternate_stream_copy_never_publishes() {
        use std::sync::atomic::AtomicBool;
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let stream = work.0.join("target:payload");
        fs::write(&stream, vec![b'x'; 12 << 20]).unwrap();
        let copying_stream = AtomicBool::new(false);
        let transaction = EditTransaction::capture(&work.target(), Some(16 << 20)).unwrap();
        let result = transaction.publish(b"replacement", Persistence::Visible, &|copied| {
            if copied > 65536 {
                copying_stream.store(true, Ordering::SeqCst);
            }
            copying_stream.load(Ordering::SeqCst)
        });
        assert!(
            copying_stream.load(Ordering::SeqCst),
            "the native copy must reach an ADS chunk"
        );
        assert!(matches!(result, Err(EditError::Cancelled)), "{result:?}");
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        assert_eq!(fs::metadata(stream).unwrap().len(), 12 << 20);
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
                    super::super::write_private(&path, &[byte; 8192])
                })
            })
            .collect();
        let mut completed = 0;
        for writer in writers {
            match writer.join().unwrap() {
                Ok(()) => completed += 1,
                // Write-through MoveFileEx may reject racing replacements;
                // each failed save must abort cleanly, never share staging.
                #[cfg(windows)]
                Err(error) if matches!(error.raw_os_error(), Some(5 | 32 | 33)) => {}
                Err(error) => panic!("{error}"),
            }
        }
        assert!(completed > 0);
        let bytes = fs::read(work.target()).unwrap();
        assert_eq!(bytes.len(), 8192);
        assert!(bytes.iter().all(|byte| *byte == bytes[0]));
        assert_eq!(fs::read(&sentinel).unwrap(), b"unrelated");
        fs::remove_file(sentinel).unwrap();
        work.only_target_remains();
    }
    #[cfg(target_vendor = "apple")]
    fn resource_fork(path: &Path, bytes: &[u8]) {
        use std::os::fd::AsRawFd;
        let file = File::open(path).unwrap();
        // SAFETY: the owned file, terminated name and complete input remain live.
        assert_eq!(
            // SAFETY: the owned descriptor, name and input buffer remain live.
            unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    c"com.apple.ResourceFork".as_ptr(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    0,
                    0,
                )
            },
            0
        );
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn resource_forks_count_toward_capture_and_publication_limits() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        resource_fork(&work.target(), &vec![b'f'; 12 << 20]);
        assert!(matches!(
            EditTransaction::capture(&work.target(), Some(1 << 20)),
            Err(EditError::TooLarge)
        ));
        let transaction = EditTransaction::capture(&work.target(), Some(16 << 20)).unwrap();
        assert!(matches!(
            transaction.publish(&vec![b'x'; 5 << 20], Persistence::Visible, &|_| false),
            Err(EditError::TooLarge)
        ));
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn cancellation_in_a_resource_fork_copy_reclaims_staging() {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::AtomicBool;
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        resource_fork(&work.target(), &vec![b'f'; 12 << 20]);
        let transaction = EditTransaction::capture(&work.target(), Some(16 << 20)).unwrap();
        let observed = AtomicBool::new(false);
        let result = transaction.publish(b"replacement", Persistence::Visible, &|copied| {
            if copied > 65536 {
                observed.store(true, Ordering::Relaxed);
                true
            } else {
                false
            }
        });
        assert!(
            observed.load(Ordering::Relaxed),
            "a fork chunk must reach the cancellation probe"
        );
        assert!(matches!(result, Err(EditError::Cancelled)));
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        let file = File::open(work.target()).unwrap();
        // SAFETY: a null output only queries the original fork's size.
        assert_eq!(
            // SAFETY: a null buffer only queries metadata size.
            unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    c"com.apple.ResourceFork".as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    0,
                )
            },
            12 << 20
        );
        work.only_target_remains();
    }

    #[cfg(unix)]
    #[test]
    fn editing_does_not_require_a_source_flock_or_write_access() {
        use std::os::unix::fs::PermissionsExt;
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        fs::set_permissions(work.target(), fs::Permissions::from_mode(0o444)).unwrap();
        let external = File::open(work.target()).unwrap();
        super::super::try_lock(&external).unwrap();
        let transaction = work.capture();
        transaction
            .publish(b"replacement", Persistence::Visible, &|_| false)
            .unwrap();
        assert_eq!(fs::read(work.target()).unwrap(), b"replacement");
        work.only_target_remains();
    }

    #[cfg(unix)]
    #[test]
    fn writers_in_the_final_window_are_detected_and_restored() {
        for replacing in [false, true] {
            let work = Workspace::new();
            fs::write(work.target(), b"original").unwrap();
            let transaction = work.capture();
            let result = transaction.publish_with_sync(
                b"ours",
                Persistence::Visible,
                File::sync_all,
                &|_| false,
                true,
                || {
                    if replacing {
                        fs::remove_file(work.target()).unwrap();
                    }
                    fs::write(work.target(), b"theirs").unwrap();
                },
            );
            assert_changed(&result);
            assert_eq!(fs::read(work.target()).unwrap(), b"theirs");
            work.only_target_remains();
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unrestored_swap_back_keeps_the_displaced_file() {
        // A second replacement between the two exchanges, or a vanished name
        // that makes the swap back fail, leaves another writer's file staged.
        for vanished in [false, true] {
            let work = Workspace::new();
            fs::write(work.target(), b"original").unwrap();
            let transaction = work.capture();
            let mut staged = transaction.stage(&|_| false).unwrap();
            staged.file.write_all(b"ours").unwrap();
            preserve_metadata(&transaction.source.as_ref().unwrap().file, &staged.file).unwrap();
            let replace = |bytes: &[u8]| {
                let incoming = work.0.join("incoming");
                fs::write(&incoming, bytes).unwrap();
                fs::rename(incoming, work.target()).unwrap();
            };
            replace(b"theirs");
            let result = transaction.commit(
                &mut staged,
                true,
                || Ok(()),
                || {},
                || {
                    if vanished {
                        fs::remove_file(work.target()).unwrap();
                    } else {
                        replace(b"newer");
                    }
                },
            );
            let Err(EditError::Conflict(EditConflict::RollbackFailed { retained })) = result else {
                panic!("{result:?}");
            };
            drop(staged);
            let kept: &[u8] = if vanished { b"theirs" } else { b"newer" };
            assert_eq!(fs::read(&retained).unwrap(), kept);
            if !vanished {
                assert_eq!(fs::read(work.target()).unwrap(), b"theirs");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn atomic_fallback_reports_the_final_change_window() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let receipt = work
            .capture()
            .publish_with_sync(
                b"ours",
                Persistence::Visible,
                File::sync_all,
                &|_| false,
                false,
                || {
                    let incoming = work.0.join("incoming");
                    fs::write(&incoming, b"theirs").unwrap();
                    fs::rename(incoming, work.target()).unwrap();
                },
            )
            .unwrap();
        assert_eq!(
            receipt.protection,
            PublicationProtection::RevalidatedAtomicReplace
        );
        assert!(receipt.limitations.change_after_check);
        assert!(receipt.limitations.writes_to_old_inode);
        assert_eq!(fs::read(work.target()).unwrap(), b"ours");
        work.only_target_remains();
    }

    #[cfg(windows)]
    #[test]
    fn acl_changes_before_the_final_check_are_never_discarded() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
            WRITE_OWNER,
        };
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let parent = open_parent(&work.0).unwrap();
        let policy = Directory::create(&parent, &work.0).unwrap();
        let authority = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC | WRITE_OWNER)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(work.target())
            .unwrap();
        let transaction = work.capture();
        let staged = transaction.stage(&|_| false).unwrap();
        assert!(!windows_policy_matches(&authority, &policy.file).unwrap());
        copy_access_policy(&policy.file, &authority).unwrap();
        let result = transaction.revalidate_staged(&staged.file);
        assert!(
            matches!(result, Err(EditError::Conflict(EditConflict::Changed))),
            "{result:?}"
        );
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        assert!(windows_policy_matches(&authority, &policy.file).unwrap());
        drop(staged);
        drop(authority);
        drop(policy);
        work.only_target_remains();
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_an_old_descriptor_cannot_change_the_published_file() {
        for exchange in [false, true] {
            let work = Workspace::new();
            fs::write(work.target(), b"original").unwrap();
            let mut old = fs::OpenOptions::new()
                .write(true)
                .open(work.target())
                .unwrap();
            let receipt = work
                .capture()
                .publish_with_sync(
                    b"ours",
                    Persistence::Visible,
                    File::sync_all,
                    &|_| false,
                    exchange,
                    || {},
                )
                .unwrap();
            old.write_all(b"late writer").unwrap();
            assert_eq!(fs::read(work.target()).unwrap(), b"ours");
            assert!(receipt.limitations.writes_to_old_inode);
            work.only_target_remains();
        }
    }

    #[cfg(unix)]
    #[test]
    fn metadata_changes_in_the_final_window_are_restored() {
        use rustix::fs::{XattrFlags, fgetxattr, fsetxattr};
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let source = File::open(work.target()).unwrap();
        fsetxattr(&source, c"user.a-edit", b"before", XattrFlags::empty()).unwrap();
        let transaction = work.capture();
        let result = transaction.publish_with_sync(
            b"ours",
            Persistence::Visible,
            File::sync_all,
            &|_| false,
            true,
            || {
                fsetxattr(&source, c"user.a-edit", b"after!", XattrFlags::empty()).unwrap();
            },
        );
        assert_changed(&result);
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        let mut value = [0; 32];
        let count = fgetxattr(
            File::open(work.target()).unwrap(),
            c"user.a-edit",
            &mut value[..],
        )
        .unwrap();
        assert_eq!(&value[..count], b"after!");
        work.only_target_remains();
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn resource_fork_changes_in_the_final_window_are_restored() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        resource_fork(&work.target(), b"before");
        let modified = fs::metadata(work.target()).unwrap().modified().unwrap();
        let result = work.capture().publish_with_sync(
            b"ours",
            Persistence::Visible,
            File::sync_all,
            &|_| false,
            true,
            || {
                resource_fork(&work.target(), b"after!");
                File::open(work.target())
                    .unwrap()
                    .set_modified(modified)
                    .unwrap();
            },
        );
        assert_changed(&result);
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        let mut bytes = [0; 32];
        let count = rustix::fs::fgetxattr(
            File::open(work.target()).unwrap(),
            c"com.apple.ResourceFork",
            &mut bytes[..],
        )
        .unwrap();
        assert_eq!(&bytes[..count], b"after!");
        work.only_target_remains();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn xattr_names_and_values_share_the_transaction_byte_limit() {
        use rustix::fs::{XattrFlags, fsetxattr};
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let source = File::open(work.target()).unwrap();
        for name in [c"user.first", c"user.second"] {
            fsetxattr(&source, name, &[b'x'; 512], XattrFlags::empty()).unwrap();
        }
        assert!(matches!(
            EditTransaction::capture(&work.target(), Some(900)),
            Err(EditError::TooLarge)
        ));
        let transaction = EditTransaction::capture(&work.target(), Some(4096)).unwrap();
        assert!(matches!(
            transaction.publish(&[b'x'; 3100], Persistence::Visible, &|_| false),
            Err(EditError::TooLarge)
        ));
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_between_xattrs_reclaims_staging_without_publication() {
        use rustix::fs::{XattrFlags, fsetxattr};
        use std::sync::atomic::AtomicBool;
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let source = File::open(work.target()).unwrap();
        for name in [c"user.first", c"user.second"] {
            fsetxattr(&source, name, &[b'x'; 512], XattrFlags::empty()).unwrap();
        }
        let observed = AtomicBool::new(false);
        let result = work
            .capture()
            .publish(b"ours", Persistence::Visible, &|copied| {
                if copied >= 512 {
                    observed.store(true, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            });
        assert!(observed.load(Ordering::Relaxed));
        assert!(matches!(result, Err(EditError::Cancelled)), "{result:?}");
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        work.only_target_remains();
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn acl_changes_in_the_final_window_are_restored() {
        let work = Workspace::new();
        fs::write(work.target(), b"original").unwrap();
        let result = work.capture().publish_with_sync(
            b"ours",
            Persistence::Visible,
            File::sync_all,
            &|_| false,
            true,
            || {
                assert!(
                    std::process::Command::new("/bin/chmod")
                        .args(["+a", "everyone allow read"])
                        .arg(work.target())
                        .status()
                        .unwrap()
                        .success()
                );
            },
        );
        assert_changed(&result);
        assert_eq!(fs::read(work.target()).unwrap(), b"original");
        let output = std::process::Command::new("/bin/ls")
            .arg("-le")
            .arg(work.target())
            .output()
            .unwrap();
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("everyone allow read")
        );
        work.only_target_remains();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staging_works_with_tmpfile_but_without_procfs_or_link_capability() {
        use std::os::unix::fs::PermissionsExt;
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        const TEST: &str =
            "fs::edit::tests::staging_works_with_tmpfile_but_without_procfs_or_link_capability";
        if let Some(root) = std::env::var_os("A_EDIT_CHROOT") {
            use rustix::fs::{AtFlags, CWD, Mode, OFlags, linkat, openat};
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStrExt;
            let root = std::ffi::CString::new(root.as_bytes()).unwrap();
            // SAFETY: this child runs only this test; root is its isolated fixture.
            assert_eq!(unsafe { libc::chroot(root.as_ptr()) }, 0);
            std::env::set_current_dir("/").unwrap();
            assert!(!Path::new("/proc").exists());
            let anonymous = openat(
                CWD,
                ".",
                OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .unwrap();
            let transaction = EditTransaction::capture(Path::new("/target"), Some(128)).unwrap();
            let mut staged = transaction.stage(&|_| false).unwrap();
            // Transfer our objects before dropping credentials. This also makes
            // AT_EMPTY_PATH require privilege on kernels that accept same-cred fds.
            for fd in [staged.file.as_raw_fd(), staged.directory.file.as_raw_fd()] {
                // SAFETY: both descriptors name this child's newly created objects.
                assert_eq!(unsafe { libc::fchown(fd, 65534, 65534) }, 0);
            }
            // SAFETY: drop this isolated child's group/user privileges permanently.
            assert_eq!(unsafe { libc::setgid(65534) }, 0);
            // SAFETY: this child permanently drops its remaining user privileges.
            assert_eq!(unsafe { libc::setuid(65534) }, 0);
            let header = Header {
                version: 0x2008_0522,
                pid: 0,
            };
            let mut capabilities = [0u32; 6];
            // SAFETY: these are the Linux v3 capget header and two data records.
            assert_eq!(
                // SAFETY: capget writes exactly the two initialized data records.
                unsafe {
                    libc::syscall(
                        libc::SYS_capget,
                        &raw const header,
                        capabilities.as_mut_ptr(),
                    )
                },
                0
            );
            assert_eq!(capabilities[0] | capabilities[3], 0);
            assert!(matches!(
                linkat(
                    &anonymous,
                    "",
                    &staged.directory.file,
                    "probe",
                    AtFlags::EMPTY_PATH
                ),
                Err(rustix::io::Errno::NOENT | rustix::io::Errno::PERM)
            ));
            staged.file.write_all(b"published").unwrap();
            staged.publish(&transaction.name).unwrap();
            drop(staged);
            assert_eq!(fs::read("/target").unwrap(), b"published");
            fs::remove_file("/target").unwrap();
            assert_eq!(fs::read_dir("/").unwrap().count(), 0);
            return;
        }
        if std::env::var_os("GITHUB_ACTIONS").is_none() {
            return;
        }
        let work = Workspace::new();
        fs::set_permissions(&work.0, fs::Permissions::from_mode(0o777)).unwrap();
        let result = std::process::Command::new("sudo")
            .args(["-n", "--preserve-env=A_EDIT_CHROOT"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env("A_EDIT_CHROOT", &work.0)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[cfg(all(test, not(miri)))]
mod termination;

#[cfg(test)]
fn publication_step(step: &str) {
    if std::env::var_os("A_EDIT_PUBLICATION_ROOT").is_some()
        && std::env::var("A_EDIT_PUBLICATION_STEP").is_ok_and(|wanted| wanted == step)
    {
        println!("EDIT_AT:{step}");
        io::stdout().flush().unwrap();
        loop {
            std::thread::park();
        }
    }
}
