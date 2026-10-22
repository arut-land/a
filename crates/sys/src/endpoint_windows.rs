use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_LOCK_VIOLATION, ERROR_NO_DATA,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{INFINITE, ResetEvent, SetEvent, WaitForSingleObject};

use crate::windows::{CompletionSource, Security, duplicate, event, ok, owned, user_sid, wide};

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn home_hash(root: &Path) -> u64 {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    // File identity unifies case variants and junctions while keeping
    // distinct homes in case-sensitive Windows directories separate.
    let identity = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(root)
        .ok()
        .and_then(|file| file.metadata().ok())
        .and_then(|meta| Some((meta.volume_serial_number()?, meta.file_index()?)));
    if let Some((volume, index)) = identity {
        let mut key = Vec::with_capacity(12);
        key.extend(volume.to_le_bytes());
        key.extend(index.to_le_bytes());
        return crate::fnv1a(&key);
    }
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    crate::fnv1a(root.to_string_lossy().to_uppercase().as_bytes())
}

pub fn address(root: &Path, _hash: u64) -> PathBuf {
    root.join("run/daemon.pid")
}

fn generation(record: &crate::crash::Record, root: &Path) -> io::Result<PathBuf> {
    let token = record
        .token
        .ok_or_else(|| io::Error::other("missing process identity"))?;
    let user = crate::fnv1a(user_sid(None)?.as_bytes());
    let home = home_hash(root);
    Ok(PathBuf::from(format!(
        r"\\.\pipe\a-{user:016x}-{home:016x}-{}-{token:016x}",
        record.pid
    )))
}

pub fn remove(_path: &Path) {}

pub struct Listener {
    path: PathBuf,
    state: Mutex<Accept>,
    event: OwnedHandle,
    nonblocking: AtomicBool,
}

struct Accept {
    file: Arc<File>,
    operation: Option<Operation>,
}

impl CompletionSource for Listener {
    fn completion_events(&self) -> io::Result<[OwnedHandle; 2]> {
        Ok([duplicate(&self.event)?, duplicate(&self.event)?])
    }
    fn nonblocking(&self) -> io::Result<()> {
        listener_nonblocking(self, true)
    }
}

fn instance(path: &Path, first: bool) -> io::Result<Arc<File>> {
    let name = wide(path.as_os_str());
    let security = Security::user_only()?;
    let attributes = security.attributes();
    let flags = PIPE_ACCESS_DUPLEX
        | FILE_FLAG_OVERLAPPED
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    // SAFETY: name and descriptor outlive the call; the checked return is owned.
    let handle = owned(unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            flags,
            PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            256 << 10,
            256 << 10,
            0,
            &raw const attributes,
        )
    })?;
    Ok(Arc::new(File::from(handle)))
}

pub fn bind(path: &Path, _link: &Path) -> io::Result<Listener> {
    let root = path
        .parent()
        .and_then(Path::parent)
        .ok_or(io::ErrorKind::InvalidInput)?;
    let name = generation(&crate::crash::Record::current(""), root)?;
    listener(instance(&name, true)?, &name)
}

fn listener(file: Arc<File>, path: &Path) -> io::Result<Listener> {
    Ok(Listener {
        path: path.to_owned(),
        state: Mutex::new(Accept {
            file,
            operation: None,
        }),
        event: event()?,
        nonblocking: AtomicBool::new(false),
    })
}

pub(crate) fn process_pipe() -> io::Result<(super::Stream, File)> {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let token = crate::process_started(std::process::id()).ok_or(io::ErrorKind::NotFound)?;
    let name = PathBuf::from(format!(
        r"\\.\pipe\a-stdio-{}-{token}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let listener = listener(instance(&name, true)?, &name)?;
    // The parent's server end is overlapped; the inherited child end uses
    // synchronous stdio, as arbitrary child programs expect.
    let child = OpenOptions::new().read(true).write(true).open(&name)?;
    Ok((super::Stream(accept(&listener)?), child))
}

pub fn adopt(_file: File, _path: &Path) -> io::Result<Listener> {
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Windows daemon startup re-listens; inherited listeners are not accepted",
    ))
}

#[allow(clippy::unnecessary_wraps, reason = "Unix changes kernel modes")]
pub fn listener_nonblocking(listener: &Listener, value: bool) -> io::Result<()> {
    listener.nonblocking.store(value, Ordering::Release);
    Ok(())
}

pub fn accept(listener: &Listener) -> io::Result<Stream> {
    let mut state = lock(&listener.state);
    loop {
        if state.operation.is_none() {
            let mut op = Operation::new(
                Arc::clone(&state.file),
                duplicate(&listener.event)?,
                Vec::new(),
            )?;
            // SAFETY: op owns the stable OVERLAPPED and event until completion.
            let success = unsafe { ConnectNamedPipe(op.file.as_raw_handle(), op.raw.as_mut()) };
            if success == 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error().map(i32::cast_unsigned) {
                    Some(ERROR_PIPE_CONNECTED) => {}
                    Some(ERROR_IO_PENDING) => op.pending = true,
                    Some(ERROR_NO_DATA) => {
                        recycle(&mut state)?;
                        continue;
                    }
                    _ => return Err(error),
                }
            }
            state.operation = Some(op);
        }
        match state
            .operation
            .as_mut()
            .unwrap()
            .finish(listener.nonblocking.load(Ordering::Acquire), None)
        {
            Ok(_) => {}
            Err(error) if eof(&error) => {
                recycle(&mut state)?;
                continue;
            }
            Err(error) => return Err(error),
        }
        let next = instance(&listener.path, false)?;
        state.operation = None;
        let connected = std::mem::replace(&mut state.file, next);
        return Stream::new(connected);
    }
}

fn recycle(state: &mut Accept) -> io::Result<()> {
    state.operation = None;
    // A client can close before ConnectNamedPipe: ERROR_NO_DATA needs an
    // explicit disconnect before the instance can listen again. By contrast,
    // ERROR_PIPE_CONNECTED is a valid connection, not an abandoned instance.
    // https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-connectnamedpipe
    // SAFETY: the listener owns this server instance and any operation is joined.
    if unsafe { DisconnectNamedPipe(state.file.as_raw_handle()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_NOT_CONNECTED.cast_signed()) {
            return Err(error);
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Stream(Arc<Connection>);

#[derive(Debug)]
struct Connection {
    file: Arc<File>,
    read_event: OwnedHandle,
    write_event: OwnedHandle,
    read: Mutex<Option<Operation>>,
    write: Mutex<Option<Operation>>,
    nonblocking: AtomicBool,
    read_closed: AtomicBool,
    write_closed: AtomicBool,
    timeout: Mutex<Option<Duration>>,
}

impl Stream {
    fn new(file: Arc<File>) -> io::Result<Self> {
        Ok(Self(Arc::new(Connection {
            file,
            read_event: event()?,
            write_event: event()?,
            read: Mutex::new(None),
            write: Mutex::new(None),
            nonblocking: AtomicBool::new(false),
            read_closed: AtomicBool::new(false),
            write_closed: AtomicBool::new(false),
            timeout: Mutex::new(None),
        })))
    }
    #[allow(clippy::unnecessary_wraps, reason = "Unix duplicates a descriptor")]
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(self.clone())
    }
}

impl CompletionSource for Stream {
    fn completion_events(&self) -> io::Result<[OwnedHandle; 2]> {
        Ok([
            duplicate(&self.0.read_event)?,
            duplicate(&self.0.write_event)?,
        ])
    }
    fn nonblocking(&self) -> io::Result<()> {
        stream_nonblocking(self, true)
    }
    fn complete_write(&self) -> io::Result<()> {
        (&*self).flush()
    }
}

pub fn connect(path: &Path) -> io::Result<Stream> {
    // The locked daemon record is also discovery: only its owner writes it.
    // A generation never reuses an old pipe namespace, even if a client
    // keeps an old instance alive after its server has exited.
    // https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createnamedpipew
    // Process objects can outlive exit while handles remain open. Only a
    // held record lock vouches for discovery, including during shutdown.
    let mut owner = OpenOptions::new().read(true).write(true).open(path)?;
    match crate::fs::try_lock(&owner) {
        Ok(()) => return Err(io::ErrorKind::NotFound.into()),
        Err(error) if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION.cast_signed()) => {}
        Err(error) => return Err(error),
    }
    let mut text = String::new();
    owner.read_to_string(&mut text)?;
    let record: crate::crash::Record = text.parse()?;
    if record.token.is_none() || !record.is_live() {
        return Err(io::ErrorKind::NotFound.into());
    }
    let root = path
        .parent()
        .and_then(Path::parent)
        .ok_or(io::ErrorKind::InvalidInput)?;
    connect_pipe(&generation(&record, root)?, Some(record.pid))
}

fn connect_pipe(path: &Path, expected: Option<u32>) -> io::Result<Stream> {
    let file = loop {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(path)
        {
            Ok(file) => break file,
            Err(e) if e.raw_os_error() == Some(231) => {
                // Wait for the server to publish its next instance, without polling.
                let name = wide(path.as_os_str());
                // SAFETY: the NUL-terminated path outlives the call.
                ok(unsafe { WaitNamedPipeW(name.as_ptr(), 5000) })?;
            }
            Err(e) => return Err(e),
        }
    };
    let mut pid = 0;
    // SAFETY: file is an open pipe, pid is a writable output.
    ok(unsafe { GetNamedPipeServerProcessId(file.as_raw_handle(), &raw mut pid) })?;
    if expected.is_some_and(|expected| expected != pid) || user_sid(Some(pid))? != user_sid(None)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "pipe server belongs to another user",
        ));
    }
    Stream::new(Arc::new(file))
}

struct Operation {
    file: Arc<File>,
    raw: Box<OVERLAPPED>,
    _event: OwnedHandle,
    buffer: Vec<u8>,
    pending: bool,
    taken: usize,
    length: usize,
}

impl std::fmt::Debug for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Operation")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

// SAFETY: the OS only touches the pinned OVERLAPPED and owned buffer while
// pending. Every inspection is serialized by the direction's mutex, and
// Drop cancels and joins the operation before freeing either allocation.
unsafe impl Send for Operation {}

impl Operation {
    fn new(file: Arc<File>, event: OwnedHandle, buffer: Vec<u8>) -> io::Result<Self> {
        // SAFETY: zero is the documented initial state; hEvent is then set.
        let mut raw: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
        raw.hEvent = event.as_raw_handle();
        // SAFETY: the event is owned and has no pending operation yet.
        ok(unsafe { ResetEvent(raw.hEvent) })?;
        Ok(Self {
            file,
            raw,
            _event: event,
            buffer,
            pending: false,
            taken: 0,
            length: 0,
        })
    }

    fn submit(&mut self, write: bool) -> io::Result<()> {
        let size = u32::try_from(self.buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: both allocations remain stable and owned until completion,
        // including when the caller drops a future or the stream.
        let success = unsafe {
            if write {
                WriteFile(
                    self.file.as_raw_handle(),
                    self.buffer.as_ptr(),
                    size,
                    null_mut(),
                    self.raw.as_mut(),
                )
            } else {
                ReadFile(
                    self.file.as_raw_handle(),
                    self.buffer.as_mut_ptr(),
                    size,
                    null_mut(),
                    self.raw.as_mut(),
                )
            }
        };
        if success == 0
            && io::Error::last_os_error().raw_os_error() != Some(ERROR_IO_PENDING.cast_signed())
        {
            return Err(io::Error::last_os_error());
        }
        self.pending = true;
        Ok(())
    }

    fn finish(&mut self, nonblocking: bool, timeout: Option<Duration>) -> io::Result<usize> {
        if !self.pending {
            return Ok(self.length);
        }
        if !nonblocking {
            let timeout = timeout.map_or(INFINITE, |d| {
                u32::try_from(d.as_millis()).unwrap_or(INFINITE - 1)
            });
            // SAFETY: the event lives with this operation.
            if unsafe { WaitForSingleObject(self.raw.hEvent, timeout) } == WAIT_TIMEOUT {
                return Err(io::ErrorKind::TimedOut.into());
            }
        }
        let mut length = 0;
        // SAFETY: the submitted operation and its file are still owned.
        let success = unsafe {
            GetOverlappedResult(
                self.file.as_raw_handle(),
                self.raw.as_mut(),
                &raw mut length,
                0,
            )
        };
        if success == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_IO_INCOMPLETE.cast_signed()) {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.pending = false;
            return Err(error);
        }
        self.pending = false;
        self.length = length as usize;
        Ok(self.length)
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        if self.pending {
            // SAFETY: cancellation alone does not release the kernel's borrow;
            // the wait joins it before raw and buffer are freed.
            unsafe {
                CancelIoEx(self.file.as_raw_handle(), self.raw.as_ref());
                let mut length = 0;
                loop {
                    if GetOverlappedResult(
                        self.file.as_raw_handle(),
                        self.raw.as_mut(),
                        &raw mut length,
                        1,
                    ) != 0
                        || io::Error::last_os_error().raw_os_error()
                            != Some(ERROR_IO_INCOMPLETE.cast_signed())
                    {
                        break;
                    }
                    // Shutdown may have signalled the event before kernel
                    // completion. Recheck the operation after resetting it;
                    // GetOverlappedResult checks completion before waiting.
                    ResetEvent(self.raw.hEvent);
                }
            }
        }
    }
}

fn eof(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(i32::cast_unsigned),
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
    )
}

impl Read for &Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let connection = &self.0;
        if bytes.is_empty() || connection.read_closed.load(Ordering::Acquire) {
            return Ok(0);
        }
        let mut read = lock(&connection.read);
        if read.is_none() {
            let mut op = Operation::new(
                Arc::clone(&connection.file),
                duplicate(&connection.read_event)?,
                vec![0; bytes.len().min(64 << 10)],
            )?;
            match op.submit(false) {
                Ok(()) => {}
                Err(e) if eof(&e) => return Ok(0),
                Err(e) => return Err(e),
            }
            *read = Some(op);
        }
        if connection.read_closed.load(Ordering::Acquire) {
            *read = None;
            return Ok(0);
        }
        let op = read.as_mut().unwrap();
        let length = match op.finish(
            connection.nonblocking.load(Ordering::Acquire),
            *lock(&connection.timeout),
        ) {
            Ok(n) => n,
            Err(e) if eof(&e) || connection.read_closed.load(Ordering::Acquire) => {
                *read = None;
                return Ok(0);
            }
            Err(e) => return Err(e),
        };
        let n = bytes.len().min(length - op.taken);
        bytes[..n].copy_from_slice(&op.buffer[op.taken..op.taken + n]);
        op.taken += n;
        if op.taken == length {
            *read = None;
        }
        Ok(n)
    }
}

impl Write for &Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let connection = &self.0;
        if connection.write_closed.load(Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut write = lock(&connection.write);
        if let Some(op) = write.as_mut() {
            op.finish(connection.nonblocking.load(Ordering::Acquire), None)?;
            *write = None;
        }
        let mut op = Operation::new(
            Arc::clone(&connection.file),
            duplicate(&connection.write_event)?,
            bytes[..bytes.len().min(64 << 10)].to_vec(),
        )?;
        op.submit(true)?;
        let accepted = op.buffer.len();
        // Report owned bytes once, when accepted. WouldBlock on a later
        // write consumes none of its buffer, which may differ on retry.
        *write = Some(op);
        Ok(accepted)
    }
    fn flush(&mut self) -> io::Result<()> {
        let connection = &self.0;
        if connection.write_closed.load(Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let mut write = lock(&connection.write);
        if let Some(op) = write.as_mut() {
            op.finish(connection.nonblocking.load(Ordering::Acquire), None)?;
            *write = None;
        }
        Ok(())
    }
}

impl Read for Stream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (&*self).read(bytes)
    }
}
impl Write for Stream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (&*self).write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&*self).flush()
    }
}

pub fn shutdown(stream: &Stream, how: Shutdown) -> io::Result<()> {
    let connection = &stream.0;
    for (close, event) in match how {
        Shutdown::Read => vec![(&connection.read_closed, &connection.read_event)],
        Shutdown::Write => vec![(&connection.write_closed, &connection.write_event)],
        Shutdown::Both => vec![
            (&connection.read_closed, &connection.read_event),
            (&connection.write_closed, &connection.write_event),
        ],
    } {
        close.store(true, Ordering::Release);
        // SAFETY: the event is owned; this also wakes a reader waiting before
        // an operation could be submitted. Cancellation wakes submitted I/O.
        ok(unsafe { SetEvent(event.as_raw_handle()) })?;
    }
    Ok(())
}

#[allow(clippy::unnecessary_wraps, reason = "Unix changes kernel modes")]
pub fn read_timeout(stream: &Stream, timeout: Option<Duration>) -> io::Result<()> {
    *lock(&stream.0.timeout) = timeout;
    Ok(())
}
#[allow(clippy::unnecessary_wraps, reason = "Unix changes kernel modes")]
pub fn stream_nonblocking(stream: &Stream, nonblocking: bool) -> io::Result<()> {
    stream.0.nonblocking.store(nonblocking, Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> crate::Home {
        crate::Home::from(
            std::env::temp_dir().join(format!("a-pipe-{name}-{}", std::process::id())),
        )
    }

    #[test]
    fn alternate_spellings_of_one_home_find_the_same_pipe() {
        let home = home("identity");
        std::fs::create_dir_all(home.root()).unwrap();
        let alternate =
            crate::Home::from(PathBuf::from(home.root().to_string_lossy().to_uppercase()));
        assert_eq!(home_hash(home.root()), home_hash(alternate.root()));
        std::fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn a_listener_has_one_owner_and_accepts_multiple_independent_clients() {
        let home = home("instances");
        let listener = home.bind().unwrap();
        assert!(instance(&listener.0.path, true).is_err());
        for text in [b"first".as_slice(), b"second".as_slice()] {
            let mut client = connect_pipe(&listener.0.path, None).unwrap();
            let mut server = accept(&listener.0).unwrap();
            client.write_all(text).unwrap();
            let mut bytes = vec![0; text.len()];
            server.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, text);
            server.write_all(text).unwrap();
            client.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, text);
        }
    }

    #[test]
    fn an_unlocked_record_never_discovers_a_retained_generation() {
        let home = home("unlocked");
        std::fs::create_dir_all(home.run_dir()).unwrap();
        let listener = home.bind().unwrap();
        std::fs::write(
            home.daemon_pid_file(),
            crate::crash::Record::current("test").to_string(),
        )
        .unwrap();
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .open(home.daemon_pid_file())
            .unwrap();
        crate::fs::lock(&owner).unwrap();
        let retained = home.endpoint().connect().unwrap();
        drop(listener.accept().unwrap());
        drop(owner);
        assert_eq!(
            home.endpoint().connect().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        drop(retained);
        drop(listener);
        std::fs::remove_dir_all(home.root()).unwrap();
    }

    #[test]
    fn abandoned_connections_never_end_the_listener() {
        let home = home("abandoned");
        let listener = home.bind().unwrap();
        listener.set_nonblocking(true).unwrap();
        for _ in 0..4 {
            drop(connect_pipe(&listener.0.path, None).unwrap());
            match accept(&listener.0) {
                Ok(stream) => drop(stream),
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
            }
            assert_eq!(
                accept(&listener.0).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
        let mut client = connect_pipe(&listener.0.path, None).unwrap();
        let mut server = accept(&listener.0).unwrap();
        client.write_all(b"still serving").unwrap();
        let mut bytes = [0; 13];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"still serving");
    }

    #[test]
    fn a_cancelled_read_keeps_its_buffer_and_completion_until_observed() {
        let home = home("cancel");
        let listener = home.bind().unwrap();
        let mut client = connect_pipe(&listener.0.path, None).unwrap();
        let mut server = accept(&listener.0).unwrap();
        stream_nonblocking(&server, true).unwrap();
        assert_eq!(
            server.read(&mut [0; 32]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        // A subsequent attempt uses a different, shorter caller buffer.
        client.write_all(b"retained").unwrap();
        let events = server.completion_events().unwrap();
        // SAFETY: this owned completion event remains open for the wait.
        let status = unsafe { WaitForSingleObject(events[0].as_raw_handle(), 5000) };
        assert_eq!(status, 0);
        let mut prefix = [0; 3];
        server.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"ret");
        let mut suffix = [0; 5];
        server.read_exact(&mut suffix).unwrap();
        assert_eq!(&suffix, b"ained");
        assert_eq!(
            server.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(server); // Drop must join cancellation before releasing the pending buffer.
    }

    #[test]
    fn a_closed_write_direction_never_waits_for_pending_completion() {
        let home = home("closed-write");
        let listener = home.bind().unwrap();
        let _client = connect_pipe(&listener.0.path, None).unwrap();
        let mut server = accept(&listener.0).unwrap();
        stream_nonblocking(&server, true).unwrap();
        loop {
            match server.write_all(&[0; 8192]) {
                Ok(()) => {}
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                    break;
                }
            }
        }
        shutdown(&server, Shutdown::Write).unwrap();
        assert_eq!(
            server.flush().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            server.write(b"later").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn shutdown_wakes_a_blocked_clone_without_waiting_for_the_peer() {
        let home = home("shutdown");
        let listener = home.bind().unwrap();
        let _client = connect_pipe(&listener.0.path, None).unwrap();
        let mut server = accept(&listener.0).unwrap();
        let clone = server.try_clone().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            tx.send(server.read(&mut [0])).unwrap();
        });
        shutdown(&clone, Shutdown::Read).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(), 0);
        reader.join().unwrap();
    }

    #[test]
    fn the_pipe_dacl_grants_access_only_to_the_current_token_sid() {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT,
        };
        use windows_sys::Win32::Security::{ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, GetAce};
        let home = home("security");
        let listener = home.bind().unwrap();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: an open pipe and writable outputs; the descriptor owns the ACL.
        let result = unsafe {
            GetSecurityInfo(
                lock(&listener.0.state).file.as_raw_handle(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &raw mut dacl,
                null_mut(),
                &raw mut descriptor,
            )
        };
        assert_eq!(result, 0);
        assert!(!dacl.is_null());
        // SAFETY: GetSecurityInfo succeeded and returned a populated ACL.
        assert_eq!(unsafe { (*dacl).AceCount }, 1);
        let mut ace = null_mut();
        // SAFETY: the ACL contains its sole ACE and ace is a writable output.
        ok(unsafe { GetAce(dacl, 0, &raw mut ace) }).unwrap();
        let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
        let mut sid = null_mut();
        // SAFETY: an allowed ACE's SID begins at SidStart and lives in descriptor.
        ok(unsafe { ConvertSidToStringSidW((&raw mut (*ace).SidStart).cast(), &raw mut sid) })
            .unwrap();
        let mut len = 0;
        // SAFETY: successful conversion returned a NUL-terminated string.
        while unsafe { *sid.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: len covers exactly the initialized UTF-16 string.
        let actual = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid, len)) };
        // SAFETY: both pointers came from APIs that allocate with LocalAlloc.
        unsafe {
            LocalFree(sid.cast());
            LocalFree(descriptor);
        }
        assert_eq!(actual, user_sid(None).unwrap());
    }
}
