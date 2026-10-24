//! Console writes are synchronous; a separate owner makes their cancellation
//! and the caller's cleanup deadline independent of a stalled console host.

use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Threading::{
    INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
};

use crate::windows::{event, ok};

struct Request {
    bytes: Box<[u8]>,
    state: Arc<Completion>,
}

struct Completion {
    event: OwnedHandle,
    interruptible: bool,
    cancelled: AtomicBool,
    result: Mutex<Option<io::Result<usize>>>,
}

struct Output {
    worker: JoinHandle<()>,
    send: mpsc::SyncSender<Request>,
    interrupt: OwnedHandle,
    failed: AtomicBool,
    active: Arc<Mutex<Option<Arc<Completion>>>>,
}

static OUTPUT: OnceLock<Output> = OnceLock::new();
static INTERRUPTS: Mutex<usize> = Mutex::new(0);

pub(in crate::terminal) fn interrupt_output() {
    let mut count = INTERRUPTS.lock().unwrap_or_else(PoisonError::into_inner);
    *count += 1;
    if let Some(output) = OUTPUT.get() {
        // SAFETY: process-lifetime manual-reset event; no borrowed buffers.
        unsafe { SetEvent(output.interrupt.as_raw_handle()) };
        let active = output.active.lock().unwrap_or_else(PoisonError::into_inner);
        if active.as_ref().is_some_and(|state| state.interruptible) {
            // SAFETY: controls cancel only this terminal worker's normal
            // frame. The gate prevents it from advancing to cleanup here.
            unsafe { CancelSynchronousIo(output.worker.as_raw_handle()) };
        }
    }
}

pub fn output_interrupted() -> bool {
    *INTERRUPTS.lock().unwrap_or_else(PoisonError::into_inner) != 0
}

pub(in crate::terminal) fn release_output() {
    let mut count = INTERRUPTS.lock().unwrap_or_else(PoisonError::into_inner);
    *count -= 1;
    if *count == 0
        && let Some(output) = OUTPUT.get()
    {
        // The same lock serializes publication, setting and resetting.
        // SAFETY: the output owner keeps this event open for the process.
        unsafe { ResetEvent(output.interrupt.as_raw_handle()) };
    }
}

pub(super) fn initialize() -> io::Result<()> {
    output().map(|_| ())
}

fn output() -> io::Result<&'static Output> {
    if OUTPUT.get().is_none() {
        let handle = io::stdout().as_handle().try_clone_to_owned()?;
        let interrupt = event()?;
        let (send, receive) = mpsc::sync_channel::<Request>(1);
        let active = Arc::new(Mutex::new(None));
        let worker_active = active.clone();
        let worker = thread::Builder::new()
            .name("terminal-output".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                for request in receive {
                    let mut active = worker_active.lock().unwrap_or_else(PoisonError::into_inner);
                    let cancelled = request.state.cancelled.load(Ordering::Acquire);
                    if !cancelled {
                        *active = Some(request.state.clone());
                    }
                    drop(active);
                    let result = if cancelled {
                        Err(io::ErrorKind::Interrupted.into())
                    } else {
                        let mut written = 0;
                        // SAFETY: the worker owns both the output handle and
                        // the bytes until WriteFile returns, even if its caller
                        // has already reached the cleanup deadline.
                        ok(unsafe {
                            WriteFile(
                                handle.as_raw_handle(),
                                request.bytes.as_ptr(),
                                u32::try_from(request.bytes.len()).unwrap(),
                                &raw mut written,
                                std::ptr::null_mut(),
                            )
                        })
                        .map(|()| written as usize)
                    };
                    // Cancellation holds this same gate, so a late retry for
                    // this frame can never cancel the next cleanup request.
                    let mut active = worker_active.lock().unwrap_or_else(PoisonError::into_inner);
                    *active = None;
                    *request
                        .state
                        .result
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some(result);
                    // SAFETY: the request owns its completion event.
                    unsafe { SetEvent(request.state.event.as_raw_handle()) };
                }
            })?;
        let pending = Output {
            worker,
            send,
            interrupt,
            failed: AtomicBool::new(false),
            active,
        };
        let count = INTERRUPTS.lock().unwrap_or_else(PoisonError::into_inner);
        if *count != 0 {
            // SAFETY: the new owner holds this completion event.
            unsafe { SetEvent(pending.interrupt.as_raw_handle()) };
        }
        // A losing initializer drops its sender, letting its idle worker exit.
        let _ = OUTPUT.set(pending);
    }
    Ok(OUTPUT.get().unwrap())
}

pub fn write_stdout(
    buf: &[u8],
    interruptible: bool,
    deadline: Option<Instant>,
) -> io::Result<usize> {
    let output = output()?;
    if output.failed.load(Ordering::Acquire) {
        return Err(io::ErrorKind::BrokenPipe.into());
    }
    if interruptible && output_interrupted() {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    if deadline.is_some_and(|end| Instant::now() >= end) {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let state = Arc::new(Completion {
        event: event()?,
        interruptible,
        cancelled: AtomicBool::new(false),
        result: Mutex::new(None),
    });
    let request = Request {
        bytes: buf[..buf.len().min(u32::MAX as usize)].into(),
        state: state.clone(),
    };
    output.send.try_send(request).map_err(|error| match error {
        mpsc::TrySendError::Full(_) => io::Error::from(io::ErrorKind::WouldBlock),
        mpsc::TrySendError::Disconnected(_) => io::Error::from(io::ErrorKind::BrokenPipe),
    })?;
    let handles = [
        state.event.as_raw_handle(),
        output.interrupt.as_raw_handle(),
    ];
    let mut cancelled = false;
    let mut finish_by = deadline;
    loop {
        if interruptible && output_interrupted() && !cancelled {
            cancelled = true;
            finish_by = Some(Instant::now() + Duration::from_millis(500));
        }
        let expired = finish_by.is_some_and(|end| Instant::now() >= end);
        if cancelled || expired {
            state.cancelled.store(true, Ordering::Release);
            // Cancel only our dedicated writer, never another component's
            // synchronous I/O. Retry until completion to close the race where
            // cancellation arrives between the worker's check and WriteFile.
            let active = output.active.lock().unwrap_or_else(PoisonError::into_inner);
            if active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &state))
            {
                // SAFETY: the owner keeps its worker handle open. The active
                // request cannot change until this cancellation call returns.
                unsafe { CancelSynchronousIo(output.worker.as_raw_handle()) };
            }
        }
        if expired {
            // A driver that fails to acknowledge cancellation must not trap
            // panic cleanup or cause the renderer to spin on a full queue.
            // The worker retains all native I/O memory until actual completion.
            output.failed.store(true, Ordering::Release);
            return Err(if cancelled {
                io::ErrorKind::WouldBlock
            } else {
                io::ErrorKind::TimedOut
            }
            .into());
        }
        let timeout = if cancelled {
            1
        } else if let Some(end) = finish_by {
            u32::try_from(end.saturating_duration_since(Instant::now()).as_millis())
                .unwrap_or(INFINITE - 1)
                .max(1)
        } else {
            INFINITE
        };
        // Idle and ordinary output wait indefinitely, with no periodic wake.
        // Only an outstanding cancellation retries on a short bounded wait.
        // SAFETY: both event handles remain owned throughout the wait.
        match unsafe {
            WaitForMultipleObjects(
                if interruptible && !cancelled { 2 } else { 1 },
                handles.as_ptr(),
                0,
                timeout,
            )
        } {
            WAIT_OBJECT_0 => {
                let result = state
                    .result
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                    .unwrap();
                // A write that completed reports its bytes even with an
                // interruption pending: they are on the screen, and a caller
                // told WouldBlock would count them unwritten and print them
                // again (the TUI's scrollback). The interruption surfaces on
                // the next call, which checks it before writing. A cancelled
                // write yields to it instead of surfacing ERROR_OPERATION_ABORTED.
                return match result {
                    Err(_) if cancelled || (interruptible && output_interrupted()) => {
                        Err(io::ErrorKind::WouldBlock.into())
                    }
                    result => result,
                };
            }
            1 | WAIT_TIMEOUT => {}
            _ => return Err(io::Error::last_os_error()),
        }
    }
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;
