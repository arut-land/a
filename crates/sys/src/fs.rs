//! Durable, private and atomic file operations. Locks need no seam: std's
//! `File::lock` is `flock` on Unix and `LockFileEx` on Windows.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

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

/// Makes the entries of `dir` durable: a file created in it survives a
/// crash. Windows makes them durable with the file itself.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(windows)]
    let _ = dir;
    Ok(())
}

/// Replaces `to` with `from` in one step: a reader sees the old file or
/// the new one. `rename`, which on Windows std makes
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`.
pub fn replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
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
    builder.create(dir)
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
            flags: VnodeEvents::WRITE | VnodeEvents::EXTEND,
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

/// Not on Windows yet, where a reader polls.
#[cfg(windows)]
pub struct WriteWatch(std::convert::Infallible);

/// Watches `path` for writes: not on Windows yet, where a reader polls.
#[cfg(windows)]
impl std::os::windows::io::AsHandle for WriteWatch {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        match self.0 {}
    }
}

#[cfg(windows)]
pub fn write_watch(_path: &Path) -> io::Result<WriteWatch> {
    Err(io::ErrorKind::Unsupported.into())
}

impl WriteWatch {
    /// Takes the writes seen so far, without waiting.
    pub fn clear(&self) {
        #[cfg(windows)]
        match self.0 {}
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
        let readable = |ms: i64| {
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: ms * 1_000_000,
            };
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
