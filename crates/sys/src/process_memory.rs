//! The native memory charge of a live process, in bytes.

use std::io;

/// The native counter used by [`charged_bytes`].
pub const METRIC: &str = if cfg!(target_os = "macos") {
    "physical footprint"
} else if cfg!(target_os = "linux") {
    "PSS"
} else {
    "private commit"
};

/// Reads the process's native charge, excluding uncharged resident pages.
///
/// macOS uses physical footprint, Linux proportional set size, and Windows
/// private commit (Task Manager's Commit Size). These are native accounting
/// policies, not interchangeable definitions of resident memory.
///
/// # Errors
/// Returns the OS error if the process cannot be inspected, or invalid data
/// if the kernel's accounting record cannot be decoded.
pub fn charged_bytes(pid: u32) -> io::Result<u64> {
    platform::charged_bytes(pid)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::{fs, io};

    pub(super) fn charged_bytes(pid: u32) -> io::Result<u64> {
        let record = fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))?;
        let value = record
            .lines()
            .find_map(|line| line.strip_prefix("Pss:"))
            .and_then(|value| value.split_whitespace().next())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process PSS"))?;
        value
            .parse::<u64>()
            .map(|kib| kib * 1024)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{io, mem::MaybeUninit};

    pub(super) fn charged_bytes(pid: u32) -> io::Result<u64> {
        let pid = i32::try_from(pid)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let mut usage = MaybeUninit::<libc::rusage_info_v4>::uninit();
        // SAFETY: the flavor matches the writable output's layout and size;
        // the kernel retains no pointer and initializes it only on success.
        if unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, usage.as_mut_ptr().cast()) }
            != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: proc_pid_rusage succeeded for this exact output type.
        Ok(unsafe { usage.assume_init() }.ri_phys_footprint)
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // PROCESS_MEMORY_COUNTERS_EX from psapi.h. PrivateUsage is the process's
    // commit charge even when its pages have left the working set.
    #[repr(C)]
    struct Counters {
        size: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        peak_paged_pool: usize,
        paged_pool: usize,
        peak_nonpaged_pool: usize,
        nonpaged_pool: usize,
        pagefile: usize,
        peak_pagefile: usize,
        private: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(process: HANDLE, counters: *mut Counters, size: u32) -> i32;
    }

    pub(super) fn charged_bytes(pid: u32) -> io::Result<u64> {
        // SAFETY: the PID is a value; no handle inheritance or pointers are requested.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: OpenProcess returned a new owned, non-null handle.
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        // SAFETY: Counters contains only integers; zero is valid for every field.
        let mut counters: Counters = unsafe { std::mem::zeroed() };
        counters.size = u32::try_from(size_of::<Counters>()).expect("process counters fit DWORD");
        // SAFETY: the handle is live and the initialized output has the declared size.
        if unsafe {
            K32GetProcessMemoryInfo(process.as_raw_handle(), &raw mut counters, counters.size)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(counters.private as u64)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use std::io;

    pub(super) fn charged_bytes(_: u32) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process memory charge",
        ))
    }
}
