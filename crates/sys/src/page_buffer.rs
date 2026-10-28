//! Reusable address space for transient bytes whose backing pages can be discarded.

use std::io;
use std::ptr::NonNull;

/// An exclusively owned reservation. Clearing discards its contents and backing
/// pages, while retaining the address range for the next burst.
#[derive(Debug)]
struct PageBuffer {
    ptr: NonNull<u8>,
    reserved: usize,
    accessible: usize,
    #[cfg(target_vendor = "apple")]
    released: usize,
    active: usize,
    len: usize,
    page: usize,
}

// SAFETY: the reservation has one owner and mutable access requires `&mut self`.
unsafe impl Send for PageBuffer {}
// SAFETY: shared access exposes only the initialized, non-discarded prefix.
unsafe impl Sync for PageBuffer {}

impl PageBuffer {
    /// Reserves at least `capacity` bytes without committing physical storage.
    pub fn new(capacity: usize) -> io::Result<Self> {
        let page = native::page_size();
        let reserved = capacity
            .checked_next_multiple_of(page)
            .filter(|&bytes| bytes > 0 && isize::try_from(bytes).is_ok())
            .ok_or(io::ErrorKind::InvalidInput)?;
        Ok(Self {
            ptr: native::reserve(reserved)?,
            reserved,
            accessible: 0,
            #[cfg(target_vendor = "apple")]
            released: 0,
            active: 0,
            len: 0,
            page,
        })
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline]
    pub fn capacity(&self) -> usize {
        self.reserved
    }

    /// Makes an initialized prefix available without moving the reservation.
    fn prepare(&mut self, len: usize) -> io::Result<()> {
        if len > self.reserved {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        let pages = len.next_multiple_of(self.page);
        if pages > self.active {
            let end = pages.max(self.active.saturating_mul(2)).min(self.reserved);
            if end > self.accessible {
                // SAFETY: this suffix is page aligned and inside the reservation.
                let start =
                    unsafe { NonNull::new_unchecked(self.ptr.as_ptr().add(self.accessible)) };
                native::commit(start, end - self.accessible)?;
                self.accessible = end;
            }
            #[cfg(target_vendor = "apple")]
            if self.active < self.released {
                // SAFETY: only the previously discarded, page-aligned prefix is reused.
                let start = unsafe { NonNull::new_unchecked(self.ptr.as_ptr().add(self.active)) };
                native::reuse(start, end.min(self.released) - self.active)?;
            }
            #[cfg(target_os = "linux")]
            {
                // SAFETY: this is the newly activated, page-aligned owned suffix.
                let start = unsafe { NonNull::new_unchecked(self.ptr.as_ptr().add(self.active)) };
                native::activate(start, end - self.active);
            }
            self.active = end;
        }
        self.len = len;
        Ok(())
    }

    #[inline]
    fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }

    /// Discards all contents. The address remains reserved until drop.
    #[inline]
    pub fn clear(&mut self) -> io::Result<()> {
        if self.active == 0 {
            self.len = 0;
            return Ok(());
        }
        native::discard(self.ptr, self.active)?;
        self.len = 0;
        #[cfg(target_vendor = "apple")]
        {
            self.released = self.released.max(self.active);
        }
        self.active = 0;
        #[cfg(windows)]
        {
            self.accessible = 0;
        }
        Ok(())
    }
}

impl AsRef<[u8]> for PageBuffer {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `len` covers only initialized bytes and is zero after discard.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}
impl AsMut<[u8]> for PageBuffer {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: as for `as_ref`, with unique access to the reservation.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}
impl Drop for PageBuffer {
    fn drop(&mut self) {
        native::release(self.ptr, self.reserved);
    }
}

#[cfg(unix)]
mod native {
    use std::io;
    use std::ptr::NonNull;

    pub fn page_size() -> usize {
        // SAFETY: sysconf reads a process-independent system constant.
        usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).expect("positive page size")
    }
    pub fn reserve(bytes: usize) -> io::Result<NonNull<u8>> {
        // Whole page-table ranges let Linux use transparent huge pages.
        #[cfg(target_os = "linux")]
        const ALIGNMENT: usize = 2 << 20;
        #[cfg(target_os = "linux")]
        let allocated = bytes
            .checked_add(ALIGNMENT)
            .filter(|&len| isize::try_from(len).is_ok())
            .ok_or(io::ErrorKind::OutOfMemory)?;
        #[cfg(not(target_os = "linux"))]
        let allocated = bytes;
        // SAFETY: anonymous private pages have no aliases; PROT_NONE reserves only.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                allocated,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr =
            NonNull::new(ptr.cast::<u8>()).ok_or_else(|| io::Error::other("null reservation"))?;
        #[cfg(target_os = "linux")]
        {
            let prefix = ptr.as_ptr().addr().wrapping_neg() & (ALIGNMENT - 1);
            // SAFETY: prefix is less than the extra no-access address space.
            let aligned = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(prefix)) };
            // SAFETY: each trimmed edge is page aligned and belongs to this reservation.
            if prefix > 0 && unsafe { libc::munmap(ptr.as_ptr().cast(), prefix) } != 0 {
                let error = io::Error::last_os_error();
                release(ptr, allocated);
                return Err(error);
            }
            let suffix = allocated - prefix - bytes;
            // SAFETY: this suffix starts immediately after the retained owned middle.
            if suffix > 0
                // SAFETY: this suffix is page aligned and remains owned.
                && unsafe { libc::munmap(aligned.as_ptr().add(bytes).cast(), suffix) } != 0
            {
                let error = io::Error::last_os_error();
                release(ptr, allocated);
                return Err(error);
            }
            Ok(aligned)
        }
        #[cfg(not(target_os = "linux"))]
        Ok(ptr)
    }
    pub fn commit(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
        // SAFETY: caller supplies page-aligned owned reserved pages.
        check(unsafe {
            libc::mprotect(
                ptr.as_ptr().cast(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
            )
        })
    }
    #[cfg(target_os = "linux")]
    pub fn activate(ptr: NonNull<u8>, bytes: usize) {
        // SAFETY: a best-effort hint on an exclusively owned, accessible range.
        unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, libc::MADV_HUGEPAGE) };
    }
    pub fn discard(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            // A later short prefix must not refault a whole old huge page.
            // SAFETY: this is the page-aligned owned range being discarded.
            unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, libc::MADV_NOHUGEPAGE) };
        }
        #[cfg(target_vendor = "apple")]
        let advice = libc::MADV_FREE_REUSABLE;
        #[cfg(target_os = "linux")]
        let advice = libc::MADV_DONTNEED;
        // SAFETY: no live references remain to these owned, page-aligned pages.
        check(unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, advice) })
    }
    #[cfg(target_vendor = "apple")]
    pub fn reuse(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
        // SAFETY: marks the previously reusable range active before any access.
        check(unsafe { libc::madvise(ptr.as_ptr().cast(), bytes, libc::MADV_FREE_REUSE) })
    }
    pub fn release(ptr: NonNull<u8>, bytes: usize) {
        // SAFETY: this is the original owned reservation, released exactly once.
        unsafe { libc::munmap(ptr.as_ptr().cast(), bytes) };
    }
    fn check(result: libc::c_int) -> io::Result<()> {
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(windows)]
mod native {
    use std::io;
    use std::ptr::NonNull;

    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE,
        VirtualAlloc, VirtualFree,
    };
    pub fn page_size() -> usize {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        let mut info = std::mem::MaybeUninit::<SYSTEM_INFO>::uninit();
        // SAFETY: GetSystemInfo initializes the entire output structure.
        unsafe {
            GetSystemInfo(info.as_mut_ptr());
            info.assume_init().dwPageSize as usize
        }
    }
    pub fn reserve(bytes: usize) -> io::Result<NonNull<u8>> {
        // SAFETY: reserves a fresh private range without committing pages.
        NonNull::new(
            unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE, PAGE_NOACCESS) }.cast(),
        )
        .ok_or_else(io::Error::last_os_error)
    }
    pub fn commit(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
        // SAFETY: commits only a page-aligned suffix of our reservation.
        if unsafe { VirtualAlloc(ptr.as_ptr().cast(), bytes, MEM_COMMIT, PAGE_READWRITE) }.is_null()
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub fn discard(ptr: NonNull<u8>, bytes: usize) -> io::Result<()> {
        // SAFETY: no live references remain; decommit releases commit charge too.
        if unsafe { VirtualFree(ptr.as_ptr().cast(), bytes, MEM_DECOMMIT) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub fn release(ptr: NonNull<u8>, _: usize) {
        // SAFETY: releases the original reservation exactly once.
        unsafe { VirtualFree(ptr.as_ptr().cast(), 0, MEM_RELEASE) };
    }
}

/// Scratch bytes: small bursts reuse heap storage; large bursts reuse reserved
/// address space and return its backing pages on clear. `maximum` is the
/// caller's existing format limit, not a residency limit.
#[derive(Debug)]
pub struct TransientBuffer {
    heap: Vec<u8>,
    start: usize,
    pages: Option<Box<PageBuffer>>,
    storage: Storage,
    maximum: usize,
    utf8: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Storage {
    Heap,
    Pages,
}

// Native discard and refault costs differ; see docs/perf/frame-buffers.md.
#[cfg(target_vendor = "apple")]
const PAGE_THRESHOLD: usize = 256 << 10;
#[cfg(target_os = "linux")]
const PAGE_THRESHOLD: usize = 4 << 20;
#[cfg(windows)]
const PAGE_THRESHOLD: usize = 1 << 20;

impl TransientBuffer {
    pub const fn new(maximum: usize) -> Self {
        Self {
            heap: Vec::new(),
            start: 0,
            pages: None,
            storage: Storage::Heap,
            maximum,
            utf8: true,
        }
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.as_ref().len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    #[inline]
    pub fn capacity(&self) -> usize {
        match self.storage {
            Storage::Heap => self.heap.capacity(),
            Storage::Pages => self.pages.as_deref().expect("active mapping").capacity(),
        }
    }
    /// Selects storage for an already-known upcoming length, without moving an
    /// existing mapping or committing the full length in advance.
    pub fn reserve_for(&mut self, len: usize) -> io::Result<()> {
        if len > self.maximum {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        if len <= PAGE_THRESHOLD || self.storage == Storage::Pages {
            return Ok(());
        }
        if self.pages.is_none() {
            self.pages = Some(Box::new(PageBuffer::new(self.maximum)?));
        }
        let pages = self.pages.as_deref_mut().expect("reservation created");
        pages.prepare(self.heap.len() - self.start)?;
        pages.as_mut().copy_from_slice(&self.heap[self.start..]);
        self.heap.clear();
        self.start = 0;
        self.storage = Storage::Pages;
        Ok(())
    }
    pub fn extend_from_slice(&mut self, bytes: &[u8]) -> io::Result<()> {
        let len = self.len();
        let end = len
            .checked_add(bytes.len())
            .ok_or(io::ErrorKind::OutOfMemory)?;
        self.reserve_for(end)?;
        match self.storage {
            Storage::Heap => {
                self.compact();
                self.heap.extend_from_slice(bytes);
            }
            Storage::Pages => {
                let pages = self.pages.as_deref_mut().expect("active mapping");
                pages.prepare(end)?;
                pages.as_mut()[len..].copy_from_slice(bytes);
            }
        }
        self.utf8 = false;
        Ok(())
    }
    #[inline]
    fn truncate(&mut self, len: usize) {
        if len < self.len() {
            self.utf8 = len == 0;
        }
        match self.storage {
            Storage::Heap => self.heap.truncate(self.start + self.len().min(len)),
            Storage::Pages => self
                .pages
                .as_deref_mut()
                .expect("active mapping")
                .truncate(len),
        }
    }
    /// Releases large backing pages and returns to the retained small buffer.
    #[inline]
    pub fn clear(&mut self) -> io::Result<()> {
        self.utf8 = true;
        self.start = 0;
        self.heap.clear();
        if self.storage == Storage::Pages {
            self.discard_pages()?;
        }
        Ok(())
    }
    /// Consumes a prefix. Receive loops retain only their unread read-ahead;
    /// a completed large burst gives its pages back before the next read.
    #[inline]
    pub fn consume(&mut self, len: usize) -> io::Result<()> {
        let len = len.min(self.len());
        if len == self.len() {
            return self.clear();
        }
        self.utf8 = false;
        match self.storage {
            Storage::Heap => self.start += len,
            Storage::Pages => {
                let pages = self.pages.as_deref_mut().expect("active mapping");
                let remaining = pages.len() - len;
                if remaining <= PAGE_THRESHOLD {
                    self.heap.extend_from_slice(&pages.as_ref()[len..]);
                    self.discard_pages()?;
                } else {
                    pages.as_mut().copy_within(len.., 0);
                    pages.truncate(remaining);
                }
            }
        }
        Ok(())
    }
    /// Appends typed text, preserving its UTF-8 guarantee without a second scan.
    pub fn push_str(&mut self, text: &str) -> io::Result<()> {
        let utf8 = self.utf8;
        self.extend_from_slice(text.as_bytes())?;
        self.utf8 = utf8;
        Ok(())
    }

    pub fn push(&mut self, byte: u8) -> io::Result<()> {
        let utf8 = self.utf8 && byte.is_ascii();
        self.extend_from_slice(&[byte])?;
        self.utf8 = utf8;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<u8> {
        let byte = *self.as_ref().last()?;
        let utf8 = self.utf8 && byte.is_ascii();
        self.truncate(self.len() - 1);
        self.utf8 = utf8;
        Some(byte)
    }

    pub fn as_str(&self) -> Result<&str, std::str::Utf8Error> {
        if self.utf8 {
            // SAFETY: typed appends and ASCII pops preserve UTF-8. Every raw
            // mutable view, raw append, or arbitrary truncation invalidates it.
            Ok(unsafe { std::str::from_utf8_unchecked(self.as_ref()) })
        } else {
            std::str::from_utf8(self.as_ref())
        }
    }

    /// Lends initialized read scratch; only `filled` commits received bytes.
    pub fn read_buffer(&mut self, len: usize) -> io::Result<ReadBuffer<'_>> {
        let kept = self.len();
        let end = kept.checked_add(len).ok_or(io::ErrorKind::OutOfMemory)?;
        if end > self.maximum {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        self.reserve_for(end)?;
        match self.storage {
            Storage::Heap => {
                self.compact();
                self.heap.resize(kept + len, 0);
            }
            Storage::Pages => self
                .pages
                .as_deref_mut()
                .expect("active mapping")
                .prepare(kept + len)?,
        }
        Ok(ReadBuffer { buffer: self, kept })
    }

    fn discard_pages(&mut self) -> io::Result<()> {
        let result = self.pages.as_deref_mut().expect("active mapping").clear();
        self.storage = Storage::Heap;
        // A failed native discard may have changed part of the range. No
        // safe slice can expose that allocation again.
        if result.is_err() {
            self.pages = None;
        }
        result
    }

    #[inline]
    fn compact(&mut self) {
        if self.start != 0 {
            self.heap.drain(..self.start);
            self.start = 0;
        }
    }
}
impl AsRef<[u8]> for TransientBuffer {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match self.storage {
            Storage::Heap => &self.heap[self.start..],
            Storage::Pages => self.pages.as_deref().expect("active mapping").as_ref(),
        }
    }
}
impl AsMut<[u8]> for TransientBuffer {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.utf8 = false;
        match self.storage {
            Storage::Heap => &mut self.heap[self.start..],
            Storage::Pages => self.pages.as_deref_mut().expect("active mapping").as_mut(),
        }
    }
}
impl std::ops::Deref for TransientBuffer {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}
impl std::ops::DerefMut for TransientBuffer {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_mut()
    }
}
impl io::Write for TransientBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.extend_from_slice(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Cancelling a read cannot make unreceived scratch visible as input.
#[derive(Debug)]
pub struct ReadBuffer<'a> {
    buffer: &'a mut TransientBuffer,
    kept: usize,
}
impl ReadBuffer<'_> {
    pub fn filled(mut self, len: usize) {
        self.kept += len;
    }
}
impl AsMut<[u8]> for ReadBuffer<'_> {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buffer.as_mut()[self.kept..]
    }
}
impl Drop for ReadBuffer<'_> {
    fn drop(&mut self) {
        self.buffer.truncate(self.kept);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg_attr(miri, ignore)]
    fn completed_bursts_return_their_pages_to_the_os() {
        #[cfg(target_os = "macos")]
        if std::env::var_os("A_TEST_PAGE_CHARGE").is_none() {
            // Charge belongs to the process, so other test threads must not
            // allocate between the before/after observations.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "page_buffer::tests::completed_bursts_return_their_pages_to_the_os",
                ])
                .env("A_TEST_PAGE_CHARGE", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let mut bytes = TransientBuffer::new(16 << 20);
        for marker in [0xa5, 0x71, 0x33] {
            bytes.reserve_for(8 << 20).unwrap();
            let mut read = bytes.read_buffer(8 << 20).unwrap();
            read.as_mut().fill(marker);
            read.filled(8 << 20);
            #[cfg(target_os = "macos")]
            let charged = charge();
            bytes.clear().unwrap();
            #[cfg(target_os = "macos")]
            assert!(charged.saturating_sub(charge()) >= 4 << 20);
            #[cfg(target_os = "linux")]
            {
                let pages = bytes.pages.as_ref().unwrap();
                let mut residency = vec![0; (8 << 20) / pages.page];
                assert_eq!(
                    // SAFETY: the owned range is page aligned, with one output byte per page.
                    unsafe {
                        libc::mincore(pages.ptr.as_ptr().cast(), 8 << 20, residency.as_mut_ptr())
                    },
                    0
                );
                assert!(residency.iter().all(|page| page & 1 == 0));
            }
            #[cfg(windows)]
            {
                use windows_sys::Win32::System::Memory::{
                    MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
                };
                let pages = bytes.pages.as_ref().unwrap();
                let mut region = std::mem::MaybeUninit::<MEMORY_BASIC_INFORMATION>::uninit();
                assert_eq!(
                    // SAFETY: the live reservation is queried into its matching output structure.
                    unsafe {
                        VirtualQuery(
                            pages.ptr.as_ptr().cast(),
                            region.as_mut_ptr(),
                            size_of::<MEMORY_BASIC_INFORMATION>(),
                        )
                    },
                    size_of::<MEMORY_BASIC_INFORMATION>()
                );
                // SAFETY: a successful VirtualQuery initialized this structure.
                let region = unsafe { region.assume_init() };
                assert_eq!(region.State, MEM_RESERVE);
                assert_eq!(region.AllocationBase, pages.ptr.as_ptr().cast());
            }
            #[cfg(target_os = "macos")]
            let idle = charge();
            bytes.reserve_for(8 << 20).unwrap();
            let mut read = bytes.read_buffer(8192).unwrap();
            read.as_mut().fill(marker);
            read.filled(8192);
            #[cfg(target_os = "macos")]
            assert!(charge().saturating_sub(idle) < 1 << 20);
            #[cfg(target_os = "linux")]
            {
                let pages = bytes.pages.as_ref().unwrap();
                let mut residency = vec![0; (8 << 20) / pages.page];
                assert_eq!(
                    // SAFETY: the reservation and the per-page output are valid.
                    unsafe {
                        libc::mincore(pages.ptr.as_ptr().cast(), 8 << 20, residency.as_mut_ptr())
                    },
                    0
                );
                let resident = residency.iter().filter(|page| **page & 1 != 0).count();
                assert!(resident * pages.page <= 8192_usize.next_multiple_of(pages.page));
            }
            #[cfg(windows)]
            {
                use windows_sys::Win32::System::Memory::{
                    MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
                };
                let pages = bytes.pages.as_ref().unwrap();
                let mut region = std::mem::MaybeUninit::<MEMORY_BASIC_INFORMATION>::uninit();
                assert_eq!(
                    // SAFETY: the queried address belongs to the live reservation.
                    unsafe {
                        VirtualQuery(
                            pages.ptr.as_ptr().add(1 << 20).cast(),
                            region.as_mut_ptr(),
                            size_of::<MEMORY_BASIC_INFORMATION>(),
                        )
                    },
                    size_of::<MEMORY_BASIC_INFORMATION>()
                );
                // SAFETY: VirtualQuery succeeded with the matching output size.
                let region = unsafe { region.assume_init() };
                assert_eq!(region.State, MEM_RESERVE);
            }
            bytes.clear().unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    fn charge() -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::uninit();
        assert_eq!(
            // SAFETY: V4 initializes the matching output structure for this live process.
            unsafe {
                libc::proc_pid_rusage(
                    std::process::id().try_into().unwrap(),
                    libc::RUSAGE_INFO_V4,
                    usage.as_mut_ptr().cast(),
                )
            },
            0
        );
        // SAFETY: proc_pid_rusage succeeded and initialized the entire V4 structure.
        unsafe { usage.assume_init().ri_phys_footprint }
    }

    #[test]
    fn unread_batches_advance_without_moving_their_bytes() {
        let mut bytes = TransientBuffer::new(8192);
        bytes.extend_from_slice(b"first second").unwrap();
        let address = bytes.as_ref().as_ptr();
        bytes.consume(6).unwrap();
        assert_eq!(bytes.as_ref(), b"second");
        assert_eq!(bytes.as_ref().as_ptr(), address.wrapping_add(6));
        bytes.extend_from_slice(b" third").unwrap();
        assert_eq!(bytes.as_ref(), b"second third");
        bytes.consume(12).unwrap();
        bytes.extend_from_slice(b"next").unwrap();
        assert_eq!(bytes.as_ref(), b"next");
    }

    #[test]
    fn raw_edits_cannot_keep_a_text_guarantee() {
        let mut bytes = TransientBuffer::new(1 << 20);
        bytes.push_str("é").unwrap();
        bytes.push(b'\n').unwrap();
        assert_eq!(bytes.as_str().unwrap(), "é\n");
        bytes.pop();
        assert_eq!(bytes.as_str().unwrap(), "é");
        bytes.pop();
        assert!(bytes.as_str().is_err());
        bytes.clear().unwrap();
        bytes.push_str("valid").unwrap();
        bytes.as_mut()[0] = 0xff;
        assert!(bytes.as_str().is_err());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn discarding_pages_preserves_the_reservation_and_hides_old_contents() {
        let mut bytes = TransientBuffer::new(8 << 20);
        let mut address = None;
        let frame_len = 5 << 20;
        for (marker, len) in [(0xa5, frame_len), (0x71, 2 << 20), (0x33, frame_len)] {
            bytes.reserve_for(frame_len).unwrap();
            let mut read = bytes.read_buffer(len).unwrap();
            read.as_mut().fill(marker);
            read.filled(len);
            if let Some(address) = address {
                assert_eq!(bytes.as_ref().as_ptr(), address);
            }
            address = Some(bytes.as_ref().as_ptr());
            assert!(bytes.as_ref().iter().all(|&b| b == marker));
            bytes.clear().unwrap();
            assert!(bytes.is_empty());
        }
    }
}
