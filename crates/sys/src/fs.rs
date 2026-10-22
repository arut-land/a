//! Durable, private and atomic file operations.
//! Unix uses flock; Windows locks a byte beyond a record so its contents
//! remain readable by clients inspecting the daemon identity.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

/// Why discovery skipped a candidate instead of opening arbitrary filesystem objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectedFile {
    NotRegular,
    OutsideRoot,
}

/// A regular file or a typed discovery rejection; IO failures remain errors.
#[derive(Debug)]
pub enum RegularFile {
    Open(File),
    Skipped(RejectedFile),
}

/// Checking metadata first avoids opening devices; nonblocking open and a
/// second check on the handle also cover replacement by a FIFO during discovery.
pub fn open_regular(path: &Path) -> io::Result<RegularFile> {
    if !fs::metadata(path)?.is_file() {
        return Ok(RegularFile::Skipped(RejectedFile::NotRegular));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open a replaced link itself, never its device target.
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Ok(RegularFile::Skipped(RejectedFile::NotRegular));
    }
    Ok(RegularFile::Open(file))
}

/// Resolving links is permitted only within the explicitly configured root.
/// The Unix walk pins each directory and refuses new links after resolution.
pub fn open_regular_within(root: &Path, path: &Path) -> io::Result<RegularFile> {
    let root = root.canonicalize()?;
    let path = path.canonicalize()?;
    let Ok(relative) = path.strip_prefix(&root) else {
        return Ok(RegularFile::Skipped(RejectedFile::OutsideRoot));
    };
    if !fs::metadata(&path)?.is_file() {
        return Ok(RegularFile::Skipped(RejectedFile::NotRegular));
    }
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, open, openat};
        let directory = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut parent = open(&root, directory, Mode::empty())?;
        let mut parts = relative.components().peekable();
        while let Some(part) = parts.next() {
            let flags = if parts.peek().is_some() {
                directory
            } else {
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC
            };
            parent = openat(&parent, part.as_os_str(), flags, Mode::empty())?;
        }
        let file = File::from(parent);
        if !file.metadata()?.is_file() {
            return Ok(RegularFile::Skipped(RejectedFile::NotRegular));
        }
        Ok(RegularFile::Open(file))
    }
    #[cfg(windows)]
    {
        let _ = relative;
        let opened = open_regular(&path)?;
        if let RegularFile::Open(file) = &opened {
            // A handle's final path detects ancestor replacement as well as
            // links in the originally supplied spelling.
            if !final_path(file)?.starts_with(&root) {
                return Ok(RegularFile::Skipped(RejectedFile::OutsideRoot));
            }
        }
        Ok(opened)
    }
}

#[cfg(windows)]
fn final_path(file: &File) -> io::Result<std::path::PathBuf> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut std::ffi::c_void,
            buffer: *mut u16,
            length: u32,
            flags: u32,
        ) -> u32;
    }
    let mut buffer = vec![0u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len())
            .map_err(|_| io::Error::other("resolved path exceeds system capacity"))?;
        // SAFETY: the file owns the handle and buffer has the advertised capacity.
        let length = unsafe {
            GetFinalPathNameByHandleW(file.as_raw_handle(), buffer.as_mut_ptr(), capacity, 0)
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if (length as usize) < buffer.len() {
            buffer.truncate(length as usize);
            return Ok(std::ffi::OsString::from_wide(&buffer).into());
        }
        buffer.resize(length as usize + 1, 0);
    }
}

/// Orders earlier writes before later commits. Apple keeps them in the
/// drive cache until a cache flush covers the promise made to a client or
/// an external effect; other platforms already flush here.
pub fn sync_data(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `file` keeps the descriptor open; this command takes no argument.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) } != -1 {
            return Ok(());
        }
    }
    file.sync_data()
}

/// Whether an ordered write still needs a separate drive-cache flush.
pub const SEPARATE_CACHE_FLUSH: bool = cfg!(target_vendor = "apple");

/// A shared cache flush can cover only files on the store's device.
pub struct FlushDomain {
    #[cfg(target_vendor = "apple")]
    device: u64,
}

impl FlushDomain {
    pub fn of(dir: &Path) -> io::Result<Self> {
        #[cfg(target_vendor = "apple")]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: fs::metadata(dir)?.dev(),
            })
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = dir;
            Ok(Self {})
        }
    }

    pub fn check(&self, file: &File) -> io::Result<()> {
        #[cfg(target_vendor = "apple")]
        {
            use std::os::unix::fs::MetadataExt;
            if file.metadata()?.dev() != self.device {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "log is outside the store's flush device",
                ));
            }
        }
        #[cfg(not(target_vendor = "apple"))]
        let _ = file;
        Ok(())
    }
}

/// Flushes the cache containing earlier ordered writes. Other platforms
/// have already flushed it with `sync_data`.
pub fn flush_cache(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `file` keeps the descriptor open; this command takes no argument.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    let _ = file;
    Ok(())
}

/// Orders a deferred commit without flushing the cache. On platforms
/// without a separate barrier it rides on the next durable commit.
pub fn order_data(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    return sync_data(file);
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = file;
        Ok(())
    }
}

/// Orders a newly created log's directory before its shared cache flush.
/// `File::sync_all` would issue its own full flush on Apple.
pub fn order_dir(dir: &Path) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    return sync_data(&File::open(dir)?);
    #[cfg(not(target_vendor = "apple"))]
    sync_dir(dir)
}

/// Creates missing directories and durably publishes each new name.
/// Existing directories are left intact; callers must use this at creation,
/// rather than trying to flush a Windows directory after adding names to it.
pub fn create_dir_all_durable(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_dir_all_durable(parent)?;
    }
    #[cfg(unix)]
    {
        match fs::create_dir(dir) {
            Ok(()) => sync_dir(
                dir.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            ),
            Err(_) if dir.is_dir() => Ok(()),
            Err(error) => Err(error),
        }
    }
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let pending = loop {
            let pending = dir.with_file_name(format!(
                ".a-directory-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&pending) {
                Ok(()) => break pending,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        };
        // A unique sibling avoids sharing staging state with another creator.
        // WRITE_THROUGH publishes the directory before files beneath it commit.
        let result = move_file(&pending, dir, false);
        if result.is_err() {
            let _ = fs::remove_dir(&pending);
        }
        match result {
            Err(_) if dir.is_dir() => Ok(()),
            result => result,
        }
    }
}

/// Makes directory entries durable on platforms with a directory barrier.
/// Windows callers must publish names with `publish` instead.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return File::open(dir)?.sync_all();
    #[cfg(windows)]
    {
        let _ = dir;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "publish names with write-through rename",
        ))
    }
}

/// Atomically replaces a name. Windows also waits for the move on disk.
pub fn replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return fs::rename(from, to);
    #[cfg(windows)]
    move_file(from, to, true)
}

#[cfg(windows)]
fn move_file(from: &Path, to: &Path, replacing: bool) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let from = crate::windows::wide(from.as_os_str());
    let to = crate::windows::wide(to.as_os_str());
    // WRITE_THROUGH waits for the move on disk; COPY_ALLOWED is deliberately
    // absent so publication cannot degrade to a cross-volume copy/delete.
    // Microsoft does not document FlushFileBuffers as a directory barrier.
    // https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw
    // SAFETY: both NUL-terminated paths remain alive for the call.
    crate::windows::ok(unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            if replacing {
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
            } else {
                MOVEFILE_WRITE_THROUGH
            },
        )
    })
}

/// Publishes an already flushed file, including its directory entry.
pub fn publish(from: &Path, to: &Path) -> io::Result<()> {
    replace(from, to)?;
    #[cfg(unix)]
    sync_dir(to.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    Ok(())
}

/// Removes the durable name before best-effort reclamation of its contents.
pub fn remove_durable(path: &Path) -> io::Result<()> {
    let tombstone = path.with_extension("removed");
    publish(path, &tombstone)?;
    fs::remove_file(tombstone)
}

/// A name whose publication must precede the log's first durable promise.
/// Windows receives this only after a flushed temporary file was published;
/// Unix retains its deferred directory barrier to share the first log flush.
pub struct Publication {
    #[cfg(unix)]
    parent: std::path::PathBuf,
}

impl Publication {
    pub fn order(&self) -> io::Result<()> {
        #[cfg(unix)]
        order_dir(&self.parent)?;
        Ok(())
    }
}

/// Opens a log, optionally creating its header in an unpublished name first.
pub fn open_log(path: &Path, header: Option<&[u8]>) -> io::Result<(File, Publication)> {
    use std::io::Write;
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    #[cfg(windows)]
    if let Some(header) = header {
        let pending = path.with_extension("creating");
        // Exclusive creation prevents two writers from sharing a temporary file.
        let mut file = private_file().open(&pending)?;
        file.write_all(header)?;
        file.sync_all()?;
        drop(file);
        move_file(&pending, path, false)?;
    }
    let mut open = OpenOptions::new();
    open.read(true).append(true);
    #[cfg(unix)]
    open.create_new(header.is_some());
    let mut file = open.open(path)?;
    #[cfg(unix)]
    if let Some(header) = header {
        file.write_all(header)?;
    }
    #[cfg(windows)]
    let _ = (&mut file, parent);
    Ok((
        file,
        Publication {
            #[cfg(unix)]
            parent: parent.to_owned(),
        },
    ))
}

/// Options that create a new file only its owner can read and write
/// (mode 0600), never opening one that exists. Windows homes are private
/// by their profile's ACL.
pub fn private_file() -> OpenOptions {
    let mut open = OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    open
}

/// Creates `dir` and its parents, entered only by their owner (mode 0700),
/// and makes an existing `dir` so.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700).create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SetFileSecurityW,
        };
        builder.create(dir)?;
        let security = crate::windows::Security::user_only()?;
        let path = crate::windows::wide(dir.as_os_str());
        // SAFETY: the path and security descriptor remain valid during the call.
        crate::windows::ok(unsafe {
            SetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                security.descriptor(),
            )
        })
    }
}

/// Fills `buf` from `file` at `offset`, as one positioned read where the
/// platform has it, without the cursor other readers share. On Windows the
/// cursor moves, which appends ignore.
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let (mut buf, mut offset) = (buf, offset);
        while !buf.is_empty() {
            match file.seek_read(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// A handle that turns readable when the file it watches is written to,
/// so a reader following the file learns of new bytes without polling: an
/// inotify instance on Linux, a kqueue of its own elsewhere on Unix.
/// Readiness is an edge; [`WriteWatch::clear`] takes what was seen, before
/// the file is read, so a write meanwhile is the next edge.
#[cfg(unix)]
pub struct WriteWatch {
    fd: std::os::fd::OwnedFd,
    /// What a kqueue watches: the file itself, open for as long as the
    /// watch is.
    #[cfg(not(target_os = "linux"))]
    _file: File,
}

#[cfg(unix)]
impl std::os::fd::AsFd for WriteWatch {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Watches `path` for writes.
#[cfg(target_os = "linux")]
pub fn write_watch(path: &Path) -> io::Result<WriteWatch> {
    use rustix::fs::inotify;
    let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
    inotify::add_watch(&fd, path, inotify::WatchFlags::MODIFY)?;
    Ok(WriteWatch { fd })
}

/// Watches `path` for writes.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn write_watch(path: &Path) -> io::Result<WriteWatch> {
    use std::os::fd::AsRawFd;

    use rustix::event::kqueue::{Event, EventFilter, EventFlags, VnodeEvents, kevent, kqueue};
    let file = File::open(path)?;
    let fd = kqueue()?;
    let watch = [Event::new(
        EventFilter::Vnode {
            vnode: file.as_raw_fd(),
            flags: VnodeEvents::WRITE
                | VnodeEvents::EXTEND
                | VnodeEvents::DELETE
                | VnodeEvents::RENAME
                | VnodeEvents::REVOKE,
        },
        EventFlags::ADD | EventFlags::CLEAR,
        std::ptr::null_mut(),
    )];
    // SAFETY: the change names `file`, which the watch keeps open for as
    // long as the queue is.
    unsafe {
        kevent(
            &fd,
            &watch,
            &mut [] as &mut [Event],
            Some(std::time::Duration::ZERO),
        )
    }?;
    Ok(WriteWatch { fd, _file: file })
}

/// A directory change notification: writes to any output in this session's
/// spill directory wake its followers, which recheck their own files.
#[cfg(windows)]
pub struct WriteWatch(Option<std::os::windows::io::OwnedHandle>);

#[cfg(windows)]
impl std::os::windows::io::AsHandle for WriteWatch {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.0.as_ref().unwrap().as_handle()
    }
}

#[cfg(windows)]
impl Drop for WriteWatch {
    fn drop(&mut self) {
        use std::os::windows::io::IntoRawHandle;
        use windows_sys::Win32::Storage::FileSystem::FindCloseChangeNotification;
        if let Some(handle) = self.0.take() {
            // SAFETY: notification handles need their matching close API,
            // after rt has unregistered the wait on this owned source.
            unsafe { FindCloseChangeNotification(handle.into_raw_handle()) };
        }
    }
}

#[cfg(windows)]
pub fn write_watch(path: &Path) -> io::Result<WriteWatch> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FindFirstChangeNotificationW,
    };
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "output has no directory"))?;
    let path = crate::windows::wide(parent.as_os_str());
    // SAFETY: path is NUL-terminated and lives across the call. WriteWatch
    // uses FindCloseChangeNotification instead of dropping the OwnedHandle.
    let handle = crate::windows::owned(unsafe {
        FindFirstChangeNotificationW(
            path.as_ptr(),
            0,
            FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE,
        )
    })?;
    Ok(WriteWatch(Some(handle)))
}

impl WriteWatch {
    /// Takes the writes seen so far, without waiting.
    pub fn clear(&self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::FindNextChangeNotification;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            let handle = self.0.as_ref().unwrap().as_raw_handle();
            // SAFETY: the watch owns this handle; rearm only after a signal.
            unsafe {
                if WaitForSingleObject(handle, 0) == 0 {
                    FindNextChangeNotification(handle);
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            let mut buf = [0; 1024];
            while matches!(rustix::io::read(&self.fd, &mut buf), Ok(n) if n > 0) {}
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            use std::mem::MaybeUninit;

            use rustix::event::kqueue::{Event, kevent};
            let mut events = [const { MaybeUninit::<Event>::uninit() }; 4];
            // SAFETY: nothing is changed; the one event watched names the
            // file this watch keeps open.
            let _ = unsafe { kevent(&self.fd, &[], &mut events, Some(std::time::Duration::ZERO)) };
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn discovery_never_opens_a_fifo_or_directory() {
        let dir = std::env::temp_dir().join(format!("a-sys-regular-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo");
        let encoded = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: encoded is a terminated path and lives through the call.
        assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
        for path in [&dir, &fifo] {
            assert!(matches!(
                open_regular(path).unwrap(),
                RegularFile::Skipped(RejectedFile::NotRegular)
            ));
        }
        let regular = dir.join("regular");
        fs::write(&regular, "instructions").unwrap();
        assert!(matches!(
            open_regular(&regular).unwrap(),
            RegularFile::Open(_)
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolved_files_and_packages_cannot_escape_the_configured_root() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("a-sys-contained-{}", std::process::id()));
        let root = dir.join("root");
        let outside = dir.join("outside");
        fs::create_dir_all(root.join("real")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("real/SKILL.md"), "inside").unwrap();
        fs::write(outside.join("SKILL.md"), "outside").unwrap();
        symlink(&outside, root.join("escape-package")).unwrap();
        symlink(outside.join("SKILL.md"), root.join("escape-file")).unwrap();
        symlink(root.join("real"), root.join("alias")).unwrap();
        for candidate in [
            root.join("escape-package/SKILL.md"),
            root.join("escape-file"),
        ] {
            assert!(matches!(
                open_regular_within(&root, &candidate).unwrap(),
                RegularFile::Skipped(RejectedFile::OutsideRoot)
            ));
        }
        assert!(matches!(
            open_regular_within(&root, &root.join("alias/SKILL.md")).unwrap(),
            RegularFile::Open(_)
        ));
        let fifo = root.join("fifo");
        let encoded = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: encoded is a terminated path and lives through the call.
        assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            open_regular_within(&root, &fifo).unwrap(),
            RegularFile::Skipped(RejectedFile::NotRegular)
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn a_file_on_another_device_is_never_admitted_to_a_flush_domain() {
        let dir = std::env::temp_dir();
        let file = File::open(&dir).unwrap();
        let domain = FlushDomain::of(&dir).unwrap();
        domain.check(&file).unwrap();
        let other = FlushDomain {
            device: domain.device ^ 1,
        };
        assert!(other.check(&file).is_err());
    }

    #[test]
    fn private_files_and_dirs_are_the_owners_alone() {
        let dir = std::env::temp_dir().join(format!("a-sys-fs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&dir.join("a/b")).unwrap();
        private_dir(&dir).unwrap();
        let file = dir.join("a/b/f");
        private_file().open(&file).unwrap();
        assert_eq!(
            [
                mode(&dir),
                mode(&dir.join("a")),
                mode(&dir.join("a/b")),
                mode(&file)
            ],
            [0o700, 0o700, 0o700, 0o600]
        );
        assert!(
            private_file().open(&file).is_err(),
            "never opens one that exists"
        );
        sync_dir(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_write_watch_turns_readable_on_each_write() {
        use std::io::Write;

        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let path = std::env::temp_dir().join(format!("a-sys-watch-{}", std::process::id()));
        let mut file = File::create(&path).unwrap();
        let watch = write_watch(&path).unwrap();
        let readable = |ms: u64| {
            let timeout = Timespec::try_from(std::time::Duration::from_millis(ms)).unwrap();
            let mut fds = [PollFd::new(&watch, PollFlags::IN)];
            poll(&mut fds, Some(&timeout)).unwrap() == 1
        };
        assert!(!readable(20), "nothing written yet");
        for _ in 0..2 {
            file.write_all(b"more\n").unwrap();
            assert!(readable(1000), "a write is seen");
            watch.clear();
            assert!(!readable(20), "what was seen is taken");
        }
        fs::remove_file(&path).unwrap();
    }
}

/// Locks a byte beyond the record, so Windows readers can still read its
/// contents while another process holds the daemon's exclusive lock.
pub fn lock(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    return file.lock();
    #[cfg(windows)]
    return windows_lock(file, false);
}

pub fn try_lock(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    return file.try_lock().map_err(io::Error::from);
    #[cfg(windows)]
    return windows_lock(file, true);
}

#[cfg(windows)]
fn windows_lock(file: &File, nonblocking: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    // SAFETY: zero initializes the union; the lock byte is far beyond data.
    let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
    operation.Anonymous.Anonymous.Offset = u32::MAX - 1;
    operation.Anonymous.Anonymous.OffsetHigh = 0x7fff_ffff;
    let flags = LOCKFILE_EXCLUSIVE_LOCK
        | if nonblocking {
            LOCKFILE_FAIL_IMMEDIATELY
        } else {
            0
        };
    // SAFETY: daemon records use synchronous handles. LockFileEx completes
    // before returning, so this stack OVERLAPPED cannot escape the call.
    crate::windows::ok(unsafe {
        LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &raw mut operation)
    })
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn a_locked_record_remains_readable_and_excludes_other_writers() {
        let root = std::env::temp_dir().join(format!("a-record-lock-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("record");
        fs::write(&path, b"daemon identity").unwrap();
        let held = File::options().read(true).write(true).open(&path).unwrap();
        lock(&held).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"daemon identity");
        let other = File::options().read(true).write(true).open(&path).unwrap();
        assert!(try_lock(&other).is_err());
        drop(held);
        try_lock(&other).unwrap();
        drop(other);
        fs::remove_dir_all(&root).unwrap();
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn a_published_log_has_its_complete_header_before_appends() {
        let root = std::env::temp_dir().join(format!("a-publish-log-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let (mut file, publication) = open_log(&path, Some(b"header\n")).unwrap();
        publication.order().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"header\n");
        assert!(open_log(&path, Some(b"replacement\n")).is_err());
        file.write_all(b"entry\n").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let (mut file, publication) = open_log(&path, None).unwrap();
        publication.order().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"header\nentry\n");
        drop(file);
        remove_durable(&path).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn durable_directory_creation_preserves_existing_contents_and_rejects_files() {
        let root = std::env::temp_dir().join(format!("a-durable-directory-{}", std::process::id()));
        let nested = root.join("children").join("space café");
        create_dir_all_durable(&nested).unwrap();
        let file = nested.join("kept");
        fs::write(&file, b"contents").unwrap();
        create_dir_all_durable(&nested).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"contents");
        assert!(create_dir_all_durable(&file.join("child")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_publication_never_replaces_the_existing_name() {
        let root = std::env::temp_dir().join(format!("a-publish-failure-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("published");
        fs::write(&path, b"old").unwrap();
        assert!(publish(&root.join("missing"), &path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        fs::remove_dir_all(root).unwrap();
    }
}
