//! Process allocator policy for the small daemon and its clients.

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
    system::release();
}

mod system {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    pub fn release() {
        // SAFETY: trimming takes no pointers and keeps every live block.
        unsafe { libc::malloc_trim(0) };
    }

    #[cfg(target_os = "macos")]
    pub fn release() {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone with no goal relieves every zone as far as it can.
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }

    #[cfg(windows)]
    pub fn release() {
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

    // musl returns freed memory as it goes.
    #[cfg(not(any(
        all(target_os = "linux", target_env = "gnu"),
        target_os = "macos",
        windows
    )))]
    pub fn release() {}
}

/// Zeroed words mapped straight from the OS: a page takes memory once it
/// is first written, and dropping the mapping gives every page back at
/// once. The allocator would keep a freed block this large for reuse (on
/// macOS, in its deferred-reclaim cache, as resident memory) after a
/// search index is built in it.
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
