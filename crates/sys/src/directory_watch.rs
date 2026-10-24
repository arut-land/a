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
use windows_sys::Win32::System::Threading::{
    CloseThreadpoolWait, CreateThreadpoolWait, PTP_CALLBACK_INSTANCE, PTP_WAIT, ResetEvent,
    SetThreadpoolWait,
};

use crate::windows::{event, ok, owned, wide};

struct Request {
    overlapped: OVERLAPPED,
    buffer: [u32; 4096],
    pending: bool,
}

// SAFETY: requests have stable heap addresses, all access is serialized by
// State's mutex; its completion wait retains the allocation through cancellation.
unsafe impl Send for Request {}

pub struct DirectoryWatch {
    state: Option<Box<State>>,
    root: PathBuf,
    recursive: bool,
}

struct State {
    directory: OwnedHandle,
    event: OwnedHandle,
    request: Mutex<Request>,
    cleanup: PTP_WAIT,
    #[cfg(test)]
    reclaimed: Option<std::sync::mpsc::Sender<()>>,
}

impl State {
    fn retire(self: Box<Self>) {
        let cleanup = self.cleanup;
        let event = self.event.as_raw_handle();
        let _ = Box::into_raw(self);
        // SAFETY: this one-shot wait is armed only after exclusive ownership
        // transfers to its callback. The event, OVERLAPPED and buffer remain
        // alive until the kernel signals completion; no timer or worker waits.
        unsafe { SetThreadpoolWait(cleanup, event, std::ptr::null()) };
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if self.cleanup != 0 {
            // SAFETY: either never armed, or inside its sole callback. Closing
            // does not wait; Windows frees the wait after this callback returns.
            unsafe { CloseThreadpoolWait(self.cleanup) };
        }
        #[cfg(test)]
        if let Some(done) = self.reclaimed.take() {
            let _ = done.send(());
        }
    }
}

unsafe extern "system" fn reclaim(
    _: PTP_CALLBACK_INSTANCE,
    context: *mut std::ffi::c_void,
    _: PTP_WAIT,
    _: u32,
) {
    // SAFETY: retire transferred this Box to a one-shot completion wait. No
    // callback is armed while DirectoryWatch can access the request.
    drop(unsafe { Box::from_raw(context.cast::<State>()) });
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
        let mut request = Request {
            // SAFETY: a zeroed OVERLAPPED with a live completion event is valid.
            overlapped: unsafe { std::mem::zeroed() },
            buffer: [0; 4096],
            pending: false,
        };
        request.overlapped.hEvent = event.as_raw_handle();
        let mut state = Box::new(State {
            directory,
            event,
            request: Mutex::new(request),
            cleanup: 0,
            #[cfg(test)]
            reclaimed: None,
        });
        // SAFETY: State's stable allocation supplies the context. This wait is
        // allocated before I/O starts, and remains unarmed until retirement.
        state.cleanup = unsafe {
            CreateThreadpoolWait(Some(reclaim), (&raw mut *state).cast(), std::ptr::null())
        };
        if state.cleanup == 0 {
            return Err(io::Error::last_os_error());
        }
        let watch = Self {
            state: Some(state),
            root: path.to_owned(),
            recursive,
        };
        watch.arm(&mut watch.state().request.lock().unwrap())?;
        Ok(watch)
    }

    fn state(&self) -> &State {
        self.state.as_ref().unwrap()
    }

    fn arm(&self, request: &mut Request) -> io::Result<()> {
        // SAFETY: the heap allocation and directory live until completion;
        // only one operation is in flight and the previous one has completed.
        let result = unsafe {
            ok(ResetEvent(self.state().event.as_raw_handle()))?;
            ok(ReadDirectoryChangesW(
                self.state().directory.as_raw_handle(),
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
        };
        request.pending = result.is_ok();
        result
    }

    /// None means the OS discarded its buffer: the consumer must fully rescan.
    pub fn take(&self) -> io::Result<Option<Vec<PathBuf>>> {
        self.take_before_rearm(|| {})
    }

    fn take_before_rearm(&self, before_rearm: impl FnOnce()) -> io::Result<Option<Vec<PathBuf>>> {
        let mut request = self.state().request.lock().unwrap();
        let mut bytes = 0;
        // SAFETY: the request owns all in-flight storage; FALSE never waits.
        let completion = ok(unsafe {
            GetOverlappedResult(
                self.state().directory.as_raw_handle(),
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
                request.pending = false;
                self.arm(&mut request)?;
                return Ok(None);
            }
            Err(error) => {
                request.pending = false;
                return Err(error);
            }
            Ok(()) => {}
        }
        request.pending = false;
        let result = self.paths(&request.buffer, bytes as usize);
        before_rearm();
        // Windows retains changes between requests in the directory handle's
        // kernel buffer. Rearm before delivering this completion to the caller.
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
        self.state().event.as_handle()
    }
}

impl Drop for DirectoryWatch {
    fn drop(&mut self) {
        let Some(mut state) = self.state.take() else {
            return;
        };
        let request = state
            .request
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !request.pending {
            return;
        }
        let mut bytes = 0;
        // SAFETY: exclusive access to this request; cancellation never frees
        // its storage. FALSE observes completion without waiting on a redirector.
        let pending = unsafe {
            CancelIoEx(
                state.directory.as_raw_handle(),
                &raw const request.overlapped,
            );
            GetOverlappedResult(
                state.directory.as_raw_handle(),
                &raw const request.overlapped,
                &raw mut bytes,
                0,
            ) == 0
                && io::Error::last_os_error().raw_os_error()
                    == Some(ERROR_IO_INCOMPLETE.cast_signed())
        };
        if pending {
            state.retire();
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
    fn delayed_cancellation_keeps_other_runtime_tasks_responsive() {
        use std::sync::{Arc, mpsc};
        use windows_sys::Win32::System::Threading::SetEvent;

        let root = tempfile::tempdir().unwrap();
        let mut watch = DirectoryWatch::new(root.path(), false).unwrap();
        let mut state = watch.state.take().unwrap();
        let request = state.request.get_mut().unwrap();
        let mut bytes = 0;
        // SAFETY: join real I/O on this test thread before substituting a
        // controlled, delayed kernel-completion signal on the same event.
        unsafe {
            CancelIoEx(
                state.directory.as_raw_handle(),
                &raw const request.overlapped,
            );
            GetOverlappedResult(
                state.directory.as_raw_handle(),
                &raw const request.overlapped,
                &raw mut bytes,
                1,
            );
            ok(ResetEvent(state.event.as_raw_handle())).unwrap();
        }
        let signal = state.event.try_clone().unwrap();
        let (done, reclaimed) = mpsc::channel();
        state.reclaimed = Some(done);
        let runtime: rt::Rt = Arc::new(rt::Threads::new(1));
        let (send, retired) = mpsc::channel();
        runtime.spawn(Box::pin(async move {
            state.retire();
            send.send(()).unwrap();
        }));
        let retirement = retired.recv_timeout(Duration::from_secs(3));
        let (send, responsive) = mpsc::channel();
        runtime.spawn(Box::pin(async move { send.send(()).unwrap() }));
        let progressed = responsive.recv_timeout(Duration::from_secs(3));
        let retained = reclaimed.try_recv().is_err();
        // SAFETY: the duplicate keeps the event live until completion delivery.
        unsafe { ok(SetEvent(signal.as_raw_handle())).unwrap() };
        retirement.unwrap();
        progressed.unwrap();
        assert!(retained, "pending OVERLAPPED storage was freed");
        reclaimed.recv_timeout(Duration::from_secs(3)).unwrap();
    }

    #[test]
    fn completions_rearm_unicode_paths_and_pending_reads_cancel_on_drop() {
        let root = std::env::temp_dir().join(format!("a-directory-watch-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let watch = DirectoryWatch::new(&root, false).unwrap();
        assert_eq!(watch.take().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        let mut between = false;
        for name in ["café", "between", "second"] {
            let path = root.join(name);
            if name != "between" {
                std::fs::write(&path, "visible").unwrap();
            }
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                let remaining = until.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "missing notification for {name}");
                let timeout = u32::try_from(remaining.as_millis()).unwrap();
                // SAFETY: watch owns the event throughout the bounded wait.
                let signalled =
                    unsafe { WaitForSingleObject(watch.state().event.as_raw_handle(), timeout) };
                assert_eq!(signalled, WAIT_OBJECT_0);
                let paths = watch
                    .take_before_rearm(|| {
                        if !between {
                            between = true;
                            std::fs::write(root.join("between"), "queued without a read").unwrap();
                        }
                    })
                    .unwrap()
                    .unwrap();
                if paths.contains(&path) {
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
