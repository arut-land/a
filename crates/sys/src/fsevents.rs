//! `FSEvents` streams, every one on a single serial dispatch queue: no
//! thread of ours, and nothing scheduled while nothing changes.

use std::ffi::{CString, c_char, c_void};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use dispatch2::{DispatchQueue, DispatchRetained};

/// What one `FSEvents` record says about its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsEventKind {
    /// Records were dropped (by the kernel or by fseventsd): anything may
    /// have changed.
    Dropped,
    /// Changes under the path were coalesced: it must be looked at whole.
    ScanTree,
    /// The watched path itself, or a directory above it, changed.
    Root,
    /// A volume was mounted or unmounted under the path.
    Mount,
    /// A directory was made at the path.
    DirCreated,
    /// Anything else about the item at the path.
    Item,
}

#[derive(Debug)]
pub struct FsEvent {
    pub path: PathBuf,
    pub kind: FsEventKind,
}

type Callback = Box<dyn Fn(Vec<FsEvent>) + Send + Sync>;

/// A started stream over one path and everything under it; dropping it
/// stops the stream on its queue, after any callback in progress.
pub struct FsEventStream(Stream);

struct Stream(*mut c_void);

// SAFETY: a stream is only stopped, invalidated and released on the queue
// its callbacks run on; the pointer is never dereferenced by Rust.
unsafe impl Send for Stream {}

impl FsEventStream {
    /// Watches `path` (which need not exist yet: its creation is a
    /// [`FsEventKind::Root`] event), calling `events` on the shared queue.
    pub fn new(
        path: &Path,
        events: impl Fn(Vec<FsEvent>) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let info: *mut Callback = Box::into_raw(Box::new(Box::new(events)));
        let context = Context {
            version: 0,
            info: info.cast(),
            retain: None,
            release: Some(release),
            describe: None,
        };
        // SAFETY: the C string outlives the call; the array retains the
        // string, released below once the stream has copied the array.
        let stream = unsafe {
            let string =
                CFStringCreateWithFileSystemRepresentation(std::ptr::null(), path.as_ptr());
            if string.is_null() {
                drop(Box::from_raw(info));
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            let paths = CFArrayCreate(
                std::ptr::null(),
                &raw const string,
                1,
                (&raw const kCFTypeArrayCallBacks).cast(),
            );
            CFRelease(string);
            if paths.is_null() {
                drop(Box::from_raw(info));
                return Err(io::Error::from(io::ErrorKind::OutOfMemory));
            }
            // The context is copied into the stream, which owns `info`
            // from here: `release` frees it with the stream.
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                callback,
                &raw const context,
                paths,
                SINCE_NOW,
                0.0,
                FILE_EVENTS | WATCH_ROOT | NO_DEFER,
            );
            CFRelease(paths);
            if stream.is_null() {
                drop(Box::from_raw(info));
                return Err(io::Error::from(io::ErrorKind::Unsupported));
            }
            stream
        };
        let queue = queue();
        // SAFETY: a live stream, scheduled on a queue that lives forever.
        let started = unsafe {
            FSEventStreamSetDispatchQueue(stream, (&raw const **queue).cast());
            FSEventStreamStart(stream) != 0
        };
        let stream = Self(Stream(stream));
        if started {
            Ok(stream)
        } else {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
    }
}

impl FsEventStream {
    /// Delivers what the stream holds, returning once its callback has
    /// had it: a stream taking over from this one misses nothing between.
    /// Never call it from the callback.
    pub fn flush(&self) {
        // SAFETY: a live, started stream; called off its queue.
        unsafe { FSEventStreamFlushSync(self.0.0) };
    }
}

impl Drop for FsEventStream {
    fn drop(&mut self) {
        let stream = Stream(std::mem::replace(&mut self.0.0, std::ptr::null_mut()));
        // On the queue, so no callback runs during or after it, and the
        // caller never waits for one.
        queue().exec_async(move || {
            let stream = stream;
            // SAFETY: the stream was started and is released exactly once.
            unsafe {
                FSEventStreamStop(stream.0);
                FSEventStreamInvalidate(stream.0);
                FSEventStreamRelease(stream.0);
            }
        });
    }
}

fn queue() -> &'static DispatchRetained<DispatchQueue> {
    static QUEUE: OnceLock<DispatchRetained<DispatchQueue>> = OnceLock::new();
    QUEUE.get_or_init(|| DispatchQueue::new("a.watch", None))
}

extern "C" fn callback(
    _: *mut c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _: *const u64,
) {
    // SAFETY: FSEvents passes `count` C paths and flags, and the context's
    // info, alive until `release`, which runs after the last callback.
    let (callback, paths, flags) = unsafe {
        (
            &*info.cast::<Callback>(),
            std::slice::from_raw_parts(paths.cast::<*const c_char>(), count),
            std::slice::from_raw_parts(flags, count),
        )
    };
    let events = paths
        .iter()
        .zip(flags)
        .map(|(&path, &flags)| FsEvent {
            // SAFETY: each path is NUL-terminated.
            path: PathBuf::from(std::ffi::OsString::from_vec(
                unsafe { std::ffi::CStr::from_ptr(path) }
                    .to_bytes()
                    .to_vec(),
            )),
            kind: kind(flags),
        })
        .collect();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(events)));
}

extern "C" fn release(info: *const c_void) {
    // SAFETY: the stream releases its context once, after its last callback.
    drop(unsafe { Box::from_raw(info.cast_mut().cast::<Callback>()) });
}

fn kind(flags: u32) -> FsEventKind {
    if flags & (USER_DROPPED | KERNEL_DROPPED | IDS_WRAPPED) != 0 {
        FsEventKind::Dropped
    } else if flags & MUST_SCAN_SUBDIRS != 0 {
        FsEventKind::ScanTree
    } else if flags & ROOT_CHANGED != 0 {
        FsEventKind::Root
    } else if flags & (MOUNT | UNMOUNT) != 0 {
        FsEventKind::Mount
    } else if flags & (ITEM_CREATED | IS_DIR) == ITEM_CREATED | IS_DIR
        && flags & (ITEM_REMOVED | ITEM_RENAMED) == 0
    {
        FsEventKind::DirCreated
    } else {
        FsEventKind::Item
    }
}

const SINCE_NOW: u64 = u64::MAX;
const NO_DEFER: u32 = 0x02;
const WATCH_ROOT: u32 = 0x04;
const FILE_EVENTS: u32 = 0x10;
const MUST_SCAN_SUBDIRS: u32 = 0x01;
const USER_DROPPED: u32 = 0x02;
const KERNEL_DROPPED: u32 = 0x04;
const IDS_WRAPPED: u32 = 0x08;
const ROOT_CHANGED: u32 = 0x20;
const MOUNT: u32 = 0x40;
const UNMOUNT: u32 = 0x80;
const ITEM_CREATED: u32 = 0x100;
const ITEM_REMOVED: u32 = 0x200;
const ITEM_RENAMED: u32 = 0x800;
const IS_DIR: u32 = 0x2_0000;

#[repr(C)]
struct Context {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    describe: Option<extern "C" fn(*const c_void) -> *const c_void>,
}

type StreamCallback =
    extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: [usize; 5];
    fn CFStringCreateWithFileSystemRepresentation(
        alloc: *const c_void,
        path: *const c_char,
    ) -> *const c_void;
    fn CFArrayCreate(
        alloc: *const c_void,
        values: *const *const c_void,
        count: isize,
        callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(object: *const c_void);
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        alloc: *const c_void,
        callback: StreamCallback,
        context: *const Context,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(stream: *mut c_void, queue: *const c_void);
    fn FSEventStreamStart(stream: *mut c_void) -> u8;
    fn FSEventStreamStop(stream: *mut c_void);
    fn FSEventStreamInvalidate(stream: *mut c_void);
    fn FSEventStreamRelease(stream: *mut c_void);
    fn FSEventStreamFlushSync(stream: *mut c_void);
}
