//! Process allocator policy for the small daemon and its clients.

/// Authority for a queued release. Loading a session abandons work not yet started.
#[derive(Debug, Default)]
pub struct ReleaseLease(idle::Lease);

/// A release that can start only while its idle generation still exists.
#[derive(Debug)]
pub struct PendingRelease(idle::Pending);

impl ReleaseLease {
    /// Whether this target has a measured policy, so owners can omit scheduling entirely.
    pub const ENABLED: bool = idle::ENABLED;

    /// Replaces the previous idle generation on platforms with a measured benefit.
    pub fn pending(&mut self) -> Option<PendingRelease> {
        self.0.pending().map(PendingRelease)
    }
}

impl PendingRelease {
    /// Returns whether maintenance was attempted. A started call cannot be interrupted.
    pub fn run(self) -> bool {
        idle::run(&self.0)
    }
}

#[cfg(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "macos", target_arch = "x86_64")
))]
mod idle {
    #[cfg(target_os = "linux")]
    use std::marker::PhantomData;
    #[cfg(target_os = "linux")]
    use std::rc::Rc;
    use std::sync::{Arc, Weak};

    // The one-session controls retained at most 3.49 MiB; waves retained over
    // 8 MiB. Round the control ceiling up, not the workload minimum down.
    #[cfg(target_os = "linux")]
    const MIN_RETAINED: usize = 4 << 20;

    pub const ENABLED: bool = true;

    #[derive(Debug, Default)]
    pub struct Lease(Option<Arc<()>>);

    #[derive(Debug)]
    pub struct Pending(Weak<()>);

    impl Lease {
        pub fn pending(&mut self) -> Option<Pending> {
            self.0 = Some(Arc::new(()));
            self.0.as_ref().map(|lease| Pending(Arc::downgrade(lease)))
        }
    }

    pub fn run(pending: &Pending) -> bool {
        if pending.0.upgrade().is_none() {
            return false;
        }
        #[cfg(target_os = "linux")]
        let Some(_priority) = Batch::new() else {
            return false;
        };
        #[cfg(target_os = "macos")]
        let _priority = crate::resources::Priority::utility();
        // SAFETY: the snapshot retains no pointers and takes allocator locks itself.
        #[cfg(target_os = "linux")]
        if unsafe { libc::mallinfo2() }.fordblks < MIN_RETAINED {
            return false;
        }
        if pending.0.upgrade().is_none() {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            // Keep the main arena warm for replay; interior pages still return.
            // SAFETY: pad retains capacity without invalidating any live allocation.
            unsafe { libc::malloc_trim(usize::MAX) };
        }
        #[cfg(target_os = "macos")]
        super::release();
        true
    }

    #[cfg(target_os = "linux")]
    struct Batch {
        restore: Option<libc::c_int>,
        _thread: PhantomData<Rc<()>>,
    }

    #[cfg(target_os = "linux")]
    impl Batch {
        fn new() -> Option<Self> {
            // SAFETY: zero selects the current worker thread.
            let previous = unsafe { libc::sched_getscheduler(0) };
            let restore = match previous {
                libc::SCHED_BATCH => None,
                libc::SCHED_OTHER => {
                    let param = libc::sched_param { sched_priority: 0 };
                    // SAFETY: BATCH requires priority zero; param lives through the call.
                    if unsafe { libc::sched_setscheduler(0, libc::SCHED_BATCH, &raw const param) }
                        != 0
                    {
                        return None;
                    }
                    Some(previous)
                }
                _ => return None,
            };
            Some(Self {
                restore,
                _thread: PhantomData,
            })
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for Batch {
        fn drop(&mut self) {
            if let Some(previous) = self.restore {
                let param = libc::sched_param { sched_priority: 0 };
                // SAFETY: restore this thread's original OTHER policy and priority.
                unsafe { libc::sched_setscheduler(0, previous, &raw const param) };
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn dropping_the_lease_abandons_queued_allocator_work() {
            let mut lease = Lease::default();
            let pending = lease.pending().unwrap();
            let (send, receive) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                receive.recv().unwrap();
                run(&pending)
            });
            drop(lease);
            send.send(()).unwrap();
            assert!(!worker.join().unwrap());
        }

        #[test]
        fn a_new_release_abandons_the_previous_generation() {
            let mut lease = Lease::default();
            let previous = lease.pending().unwrap();
            let _next = lease.pending();
            assert!(!run(&previous));
        }

        #[test]
        #[cfg_attr(miri, ignore)]
        #[cfg(target_os = "linux")]
        fn only_retired_capacity_above_the_measured_floor_is_released() {
            if std::env::var_os("A_RELEASE_TEST_CHILD").is_none() {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "memory::idle::tests::only_retired_capacity_above_the_measured_floor_is_released", "--nocapture", "--test-threads=1"])
                    .env("A_RELEASE_TEST_CHILD", "1")
                    .output().unwrap();
                assert!(
                    output.status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            let mut lease = Lease::default();
            // SAFETY: the allocator owns the snapshot; no pointers escape.
            assert!(unsafe { libc::mallinfo2() }.fordblks < MIN_RETAINED);
            assert!(!run(&lease.pending().unwrap()));
            let mut buffers = Vec::with_capacity(256);
            let mut anchors = Vec::with_capacity(256);
            for _ in 0..256 {
                buffers.push(vec![17_u8; 96 << 10]);
                anchors.push(vec![29_u8; 8 << 10]);
            }
            std::hint::black_box(&buffers);
            drop(buffers);
            // SAFETY: as above; live anchors keep the retired chunks below the top.
            assert!(unsafe { libc::mallinfo2() }.fordblks >= MIN_RETAINED);
            let before = crate::process_memory::charged_bytes(std::process::id()).unwrap();
            assert!(run(&lease.pending().unwrap()));
            let after = crate::process_memory::charged_bytes(std::process::id()).unwrap();
            assert!(
                after < before,
                "retired pages remain charged: {before} -> {after}"
            );
            assert!(anchors.iter().flatten().all(|byte| *byte == 29));
        }

        #[test]
        #[cfg_attr(miri, ignore)]
        #[cfg(target_os = "linux")]
        fn allocator_maintenance_restores_the_workers_scheduling_policy() {
            std::thread::spawn(|| {
                // SAFETY: zero selects the current test worker.
                let previous = unsafe { libc::sched_getscheduler(0) };
                {
                    let _priority = Batch::new().expect("normal worker can enter batch policy");
                    // SAFETY: querying the same live thread.
                    assert_eq!(unsafe { libc::sched_getscheduler(0) }, libc::SCHED_BATCH);
                }
                // SAFETY: querying the same live thread after scoped maintenance.
                assert_eq!(unsafe { libc::sched_getscheduler(0) }, previous);
            })
            .join()
            .unwrap();
        }
    }
}

#[cfg(not(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "macos", target_arch = "x86_64")
)))]
mod idle {
    pub const ENABLED: bool = false;

    #[derive(Debug, Default)]
    pub struct Lease(Option<Pending>);

    #[derive(Debug)]
    pub struct Pending(std::convert::Infallible);

    impl Lease {
        pub fn pending(&mut self) -> Option<Pending> {
            self.0.take()
        }
    }

    pub fn run(pending: &Pending) -> bool {
        match pending.0 {}
    }
}

/// Bounds per-thread arena growth before the process starts workers.
/// Other allocators already manage their own arena policy.
pub fn small_footprint() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // glibc can fault a 2 MiB huge page for each mostly empty arena.
        // Two arenas retain concurrency without scaling idle RSS by thread count.
        // SAFETY: mallopt changes allocator policy and retains no pointers.
        unsafe { libc::mallopt(libc::M_ARENA_MAX, 2) };
    }
}

/// Returns unused allocator pages to the operating system without moving live allocations.
pub fn release() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: trimming takes no pointers and keeps every live block.
        unsafe { libc::malloc_trim(0) };
    }

    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone with no goal relieves every zone as far as it can.
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Memory::{HeapOptimizeResources, HeapSetInformation};
        use windows_sys::Win32::System::SystemServices::{
            HEAP_OPTIMIZE_RESOURCES_CURRENT_VERSION, HEAP_OPTIMIZE_RESOURCES_INFORMATION,
        };
        let info = HEAP_OPTIMIZE_RESOURCES_INFORMATION {
            Version: HEAP_OPTIMIZE_RESOURCES_CURRENT_VERSION,
            Flags: 0,
        };
        // SAFETY: a null heap applies to every heap; `info` outlives the call.
        unsafe {
            HeapSetInformation(
                std::ptr::null_mut(),
                HeapOptimizeResources,
                (&raw const info).cast(),
                size_of_val(&info),
            )
        };
    }
}

/// Zeroed words mapped straight from the OS: a page takes memory once it
/// is first written, and dropping the mapping gives every page back at
/// once. The allocator would keep a freed block this large for reuse (on
/// macOS, in its deferred-reclaim cache, as resident memory) after a
/// search index is built in it.
#[derive(Debug)]
pub struct ZeroedWords {
    ptr: std::ptr::NonNull<u64>,
    len: usize,
}

// SAFETY: the mapping is owned exclusively, like a `Box<[u64]>`.
unsafe impl Send for ZeroedWords {}
// SAFETY: shared access is only through `&[u64]`.
unsafe impl Sync for ZeroedWords {}

impl ZeroedWords {
    /// `len` zeroed words, or the OS's refusal.
    pub fn new(len: usize) -> std::io::Result<Self> {
        let bytes = len
            .checked_mul(size_of::<u64>())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        if bytes == 0 {
            return Ok(Self {
                ptr: std::ptr::NonNull::dangling(),
                len: 0,
            });
        }
        let ptr = map(bytes)?;
        Ok(Self { ptr, len })
    }
}

impl std::ops::Deref for ZeroedWords {
    type Target = [u64];

    fn deref(&self) -> &[u64] {
        // SAFETY: `ptr` maps `len` zero-initialised words owned by `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl std::ops::DerefMut for ZeroedWords {
    fn deref_mut(&mut self) -> &mut [u64] {
        // SAFETY: as in `deref`, and `&mut self` makes the access unique.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for ZeroedWords {
    fn drop(&mut self) {
        if self.len > 0 {
            unmap(self.ptr, self.len * size_of::<u64>());
        }
    }
}

#[cfg(unix)]
fn map(bytes: usize) -> std::io::Result<std::ptr::NonNull<u64>> {
    // SAFETY: a private anonymous mapping aliases nothing; the kernel
    // zero-fills its pages on first touch.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    std::ptr::NonNull::new(ptr.cast()).ok_or_else(|| std::io::Error::other("mapped at null"))
}

#[cfg(unix)]
fn unmap(ptr: std::ptr::NonNull<u64>, bytes: usize) {
    // SAFETY: `ptr` and `bytes` are exactly a mapping `map` made, unmapped once.
    unsafe { libc::munmap(ptr.as_ptr().cast(), bytes) };
}

#[cfg(windows)]
fn map(bytes: usize) -> std::io::Result<std::ptr::NonNull<u64>> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };
    // SAFETY: a fresh private allocation aliases nothing; committed pages
    // are zero-filled and take physical memory on first touch.
    let ptr = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            bytes,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        )
    };
    std::ptr::NonNull::new(ptr.cast()).ok_or_else(std::io::Error::last_os_error)
}

#[cfg(windows)]
fn unmap(ptr: std::ptr::NonNull<u64>, _: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    // SAFETY: `ptr` is the base of an allocation `map` made, released once.
    unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_words_start_zeroed_and_hold_what_is_written() {
        let mut words = ZeroedWords::new(1 << 20).unwrap();
        assert!(words.iter().all(|w| *w == 0));
        words[12345] = 7;
        assert_eq!(words[12345], 7);
        assert!(ZeroedWords::new(0).unwrap().is_empty());
    }
}
