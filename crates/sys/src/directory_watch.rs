//! Overlapped directory notifications, owned through cancellation completion.
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{ERROR_IO_INCOMPLETE, ERROR_NOTIFY_ENUM_DIR};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::ResetEvent;

use crate::windows::{event, ok, owned, wide};

struct Request {
    overlapped: OVERLAPPED,
    buffer: [u32; 4096],
}

// SAFETY: requests have stable heap addresses, all access is serialized by
// DirectoryWatch's mutex, and Drop joins cancellation before freeing them.
unsafe impl Send for Request {}

pub struct DirectoryWatch {
    directory: OwnedHandle,
    event: OwnedHandle,
    root: PathBuf,
    recursive: bool,
    request: Mutex<Box<Request>>,
}

impl DirectoryWatch {
    pub fn new(path: &Path, recursive: bool) -> io::Result<Self> {
        let path_wide = wide(path.as_os_str());
        // SAFETY: terminated path, no inherited handle; the result is checked.
        let directory = owned(unsafe {
            CreateFileW(
                path_wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        })?;
        let event = event()?;
        let mut request = Box::new(Request {
            // SAFETY: a zeroed OVERLAPPED with a live completion event is valid.
            overlapped: unsafe { std::mem::zeroed() },
            buffer: [0; 4096],
        });
        request.overlapped.hEvent = event.as_raw_handle();
        let watch = Self {
            directory,
            event,
            root: path.to_owned(),
            recursive,
            request: Mutex::new(request),
        };
        watch.arm(&mut watch.request.lock().unwrap())?;
        Ok(watch)
    }

    fn arm(&self, request: &mut Request) -> io::Result<()> {
        // SAFETY: the heap allocation and directory live until completion;
        // only one operation is in flight and the previous one has completed.
        unsafe {
            ok(ResetEvent(self.event.as_raw_handle()))?;
            ok(ReadDirectoryChangesW(
                self.directory.as_raw_handle(),
                request.buffer.as_mut_ptr().cast(),
                16 * 1024,
                i32::from(self.recursive),
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES,
                std::ptr::null_mut(),
                &raw mut request.overlapped,
                None,
            ))
        }
    }

    /// None means the OS discarded its buffer: the consumer must fully rescan.
    pub fn take(&self) -> io::Result<Option<Vec<PathBuf>>> {
        let mut request = self.request.lock().unwrap();
        let mut bytes = 0;
        // SAFETY: the request owns all in-flight storage; FALSE never waits.
        let completion = ok(unsafe {
            GetOverlappedResult(
                self.directory.as_raw_handle(),
                &raw const request.overlapped,
                &raw mut bytes,
                0,
            )
        });
        match completion {
            Err(error) if error.raw_os_error() == Some(ERROR_IO_INCOMPLETE.cast_signed()) => {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            Err(error) if error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR.cast_signed()) => {
                self.arm(&mut request)?;
                return Ok(None);
            }
            Err(error) => return Err(error),
            Ok(()) => {}
        }
        let result = self.paths(&request.buffer, bytes as usize);
        self.arm(&mut request)?;
        Ok(result)
    }

    fn paths(&self, buffer: &[u32], bytes: usize) -> Option<Vec<PathBuf>> {
        if bytes == 0 || bytes > size_of_val(buffer) {
            return None;
        }
        // SAFETY: buffer is initialized, and bytes is bounded by its allocation.
        let buffer = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), bytes) };
        let mut paths = Vec::new();
        let mut offset = 0;
        loop {
            let header = buffer.get(offset..offset + 12)?;
            let next = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
            let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
            if length % 2 != 0 {
                return None;
            }
            let name: Vec<u16> = buffer
                .get(offset + 12..offset + 12 + length)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            paths.push(self.root.join(std::ffi::OsString::from_wide(&name)));
            if next == 0 {
                break;
            }
            if next < 12 + length || next > bytes - offset {
                return None;
            }
            offset += next;
        }
        Some(paths)
    }
}

impl AsHandle for DirectoryWatch {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.event.as_handle()
    }
}

impl Drop for DirectoryWatch {
    fn drop(&mut self) {
        let request = self
            .request
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bytes = 0;
        // SAFETY: cancellation is joined while the directory, event and pinned
        // request still live. ERROR_NOT_FOUND also permits joining completion.
        unsafe {
            CancelIoEx(
                self.directory.as_raw_handle(),
                &raw const request.overlapped,
            );
            GetOverlappedResult(
                self.directory.as_raw_handle(),
                &raw const request.overlapped,
                &raw mut bytes,
                1,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    #[test]
    fn completions_rearm_unicode_paths_and_pending_reads_cancel_on_drop() {
        let root = std::env::temp_dir().join(format!("a-directory-watch-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let watch = DirectoryWatch::new(&root, false).unwrap();
        assert_eq!(watch.take().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        for name in ["café", "second"] {
            let path = root.join(name);
            std::fs::write(&path, "visible").unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                let remaining = until.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "missing notification for {name}");
                let timeout = u32::try_from(remaining.as_millis()).unwrap();
                // SAFETY: watch owns the event throughout the bounded wait.
                let signalled =
                    unsafe { WaitForSingleObject(watch.event.as_raw_handle(), timeout) };
                assert_eq!(signalled, WAIT_OBJECT_0);
                if watch.take().unwrap().unwrap().contains(&path) {
                    break;
                }
            }
        }
        assert!(watch.paths(&[0; 4096], 0).is_none());
        assert!(watch.paths(&[0; 4096], 4).is_none());
        assert!(watch.paths(&[0; 4096], 16 * 1024 + 1).is_none());
        drop(watch);
        std::fs::remove_dir_all(root).unwrap();
    }
}
