//! File rotation and stderr redirection at the platform seam.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Moves `path` to `path.1` once it holds more than `max` bytes, shifting
/// older ones up and dropping the one past `keep`. Returns whether it did.
pub fn rotate(path: &Path, max: u64, keep: usize) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(meta) if meta.len() > max => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    if keep == 0 {
        fs::remove_file(path)?;
        return Ok(true);
    }
    for n in (1..keep).rev() {
        match fs::rename(numbered(path, n), numbered(path, n + 1)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    fs::rename(path, numbered(path, 1))?;
    Ok(true)
}

fn numbered(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Discards detached daemon stderr: arbitrary library errors and panic
/// payloads must never enter the typed diagnostic log.
pub fn discard_stderr() -> io::Result<()> {
    stderr_to(&fs::OpenOptions::new().write(true).open(null_device())?)
}

#[cfg(unix)]
fn null_device() -> &'static str {
    "/dev/null"
}
#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}

#[cfg(unix)]
fn stderr_to(log: &fs::File) -> io::Result<()> {
    rustix::stdio::dup2_stderr(log)?;
    Ok(())
}

#[cfg(windows)]
fn stderr_to(log: &fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
    // SetStdHandle does not duplicate or own its input. Keep our duplicate
    // alive until the next redirection, closing the previous one only then.
    static STDERR: std::sync::Mutex<Option<fs::File>> = std::sync::Mutex::new(None);
    let mut current = STDERR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let duplicate = log.try_clone()?;
    // A standard writer must finish before its old borrowed handle closes.
    let _writer = io::stderr().lock();
    // SAFETY: duplicate remains owned in STDERR after the call.
    crate::windows::ok(unsafe { SetStdHandle(STD_ERROR_HANDLE, duplicate.as_raw_handle()) })?;
    *current = Some(duplicate);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_by_size_and_keeps_a_few() {
        let dir = std::env::temp_dir().join(format!("a-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("daemon.log");
        assert!(!rotate(&log, 4, 2).unwrap(), "no log yet");
        for text in ["first", "second", "third"] {
            fs::write(&log, text).unwrap();
            assert!(rotate(&log, 4, 2).unwrap());
        }
        fs::write(&log, "tiny").unwrap();
        assert!(!rotate(&log, 4, 2).unwrap());
        let read = |n| fs::read_to_string(numbered(&log, n)).ok();
        let kept = (read(1), read(2), read(3));
        let live = fs::read_to_string(&log).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(live, "tiny");
        assert_eq!(kept, (Some("third".into()), Some("second".into()), None));
    }
}
