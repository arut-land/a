//! Durable, private and atomic file operations. Locks need no seam: std's
//! `File::lock` is `flock` on Unix and `LockFileEx` on Windows.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

/// Makes what was written to `file` durable, in write order. On Apple this
/// is a write barrier (`F_BARRIERFSYNC`, ~0.25 ms) rather than the cache
/// flush `sync_data` does there (`F_FULLFSYNC`, ~2.6 ms): writes still land
/// in order, but the newest may be lost on power loss. A filesystem without
/// barriers gets the full sync; Windows gets `FlushFileBuffers`.
pub fn sync_data(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `fcntl` on a descriptor `file` owns and keeps open; the
        // command takes no argument.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) } != -1 {
            return Ok(());
        }
    }
    file.sync_data()
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
}
