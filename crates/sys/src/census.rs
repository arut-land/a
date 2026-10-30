//! Opt-in native heap observations for fixture daemons, excluded from shipped builds.

use std::io::{self, Write};

/// Starts a census endpoint only in a daemon with an explicit output directory.
/// The harness owns the directory and requests each observation over loopback.
pub fn initialize() {
    if std::env::args_os()
        .nth(1)
        .is_none_or(|arg| arg != "__daemon")
    {
        return;
    }
    let Some(home) = std::env::var_os("A_HOME") else {
        return;
    };
    // Daemon startup deliberately does not inherit arbitrary client environment.
    let directory = std::path::PathBuf::from(home).join("alloc-census");
    if !directory.is_dir() {
        return;
    }
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("fixture census must bind loopback");
    std::fs::write(
        directory.join(format!("{}.port", std::process::id())),
        listener
            .local_addr()
            .expect("bound listener has a local address")
            .port()
            .to_string(),
    )
    .expect("fixture census must publish its endpoint");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("fixture census must accept observation");
            let result = match snapshot() {
                Ok(value) => value,
                Err(error) => serde_json::json!({"error": error.to_string()}),
            };
            writeln!(stream, "{result}").expect("fixture census must deliver observation");
        }
    });
}

fn snapshot() -> io::Result<serde_json::Value> {
    let pid = std::process::id();
    let before = crate::process_memory::charged_bytes(pid)?;
    let mut value = native_snapshot()?;
    value["pid"] = pid.into();
    value["charged_before_bytes"] = before.into();
    value["charged_after_bytes"] = crate::process_memory::charged_bytes(pid)?.into();
    Ok(value)
}

fn native_snapshot() -> io::Result<serde_json::Value> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::ffi::CStr;
        let mut buffer = std::ptr::null_mut();
        let mut length = 0;
        // SAFETY: both output slots live until the stream is closed. libc owns
        // the buffer until fclose; free below releases it exactly once.
        let stream = unsafe { libc::open_memstream(&raw mut buffer, &raw mut length) };
        if stream.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: stream is a writable, exclusively owned libc FILE.
        let status = unsafe { libc::malloc_info(0, stream) };
        // SAFETY: stream is open and is closed exactly once here.
        let closed = unsafe { libc::fclose(stream) };
        let result = if status == 0 && closed == 0 {
            // SAFETY: successful fclose publishes a NUL-terminated buffer.
            let xml = unsafe { CStr::from_ptr(buffer) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: mallinfo2 takes no pointers and snapshots allocator counters.
            let info = unsafe { libc::mallinfo2() };
            Ok(serde_json::json!({"allocator": "glibc", "malloc_info": xml,
                "mallinfo2_allocated_bytes": info.uordblks + info.hblkhd,
                "mallinfo2_free_bytes": info.fordblks}))
        } else {
            Err(io::Error::last_os_error())
        };
        // SAFETY: open_memstream allocates the buffer with malloc; null is allowed.
        unsafe { libc::free(buffer.cast()) };
        result
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Memory::{GetProcessHeaps, HEAP_SUMMARY, HeapSummary};
        // SAFETY: zero length permits a null output pointer.
        let count = unsafe { GetProcessHeaps(0, std::ptr::null_mut()) };
        if count == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut heaps = vec![std::ptr::null_mut(); count as usize];
        // SAFETY: heaps has capacity for count handles; the API retains no pointer.
        let actual = unsafe { GetProcessHeaps(count, heaps.as_mut_ptr()) };
        if actual == 0 || actual > count {
            return Err(io::Error::other("process heap list changed during census"));
        }
        let mut rows = Vec::new();
        for heap in &heaps[..actual as usize] {
            let mut summary = HEAP_SUMMARY {
                cb: u32::try_from(size_of::<HEAP_SUMMARY>()).expect("HEAP_SUMMARY fits DWORD"),
                ..Default::default()
            };
            // SAFETY: each handle belongs to this process; the sized output is writable.
            if unsafe { HeapSummary(*heap, 0, &raw mut summary) } == 0 {
                return Err(io::Error::last_os_error());
            }
            rows.push(serde_json::json!({"heap": *heap as usize,
                "allocated_bytes": summary.cbAllocated, "committed_bytes": summary.cbCommitted,
                "reserved_bytes": summary.cbReserved}));
        }
        Ok(serde_json::json!({"allocator": "windows-heaps", "heaps": rows}))
    }
    #[cfg(not(any(windows, all(target_os = "linux", target_env = "gnu"))))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native census requires glibc or Windows",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(windows, all(target_os = "linux", target_env = "gnu")))]
    #[test]
    fn freed_large_allocation_is_no_longer_counted_as_allocated() {
        fn allocated() -> u64 {
            let value = snapshot().unwrap();
            if value["allocator"] == "glibc" {
                value["mallinfo2_allocated_bytes"].as_u64().unwrap()
            } else {
                value["heaps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|heap| heap["allocated_bytes"].as_u64().unwrap())
                    .sum()
            }
        }
        let before = allocated();
        let held = vec![42_u8; 4 * 1024 * 1024];
        std::hint::black_box(&held);
        let during = allocated();
        drop(held);
        let after = allocated();
        assert!(during >= before + 3 * 1024 * 1024, "{before} -> {during}");
        assert!(during >= after + 3 * 1024 * 1024, "{during} -> {after}");
    }

    #[test]
    fn census_identifies_the_native_allocator_or_reports_unsupported() {
        match snapshot() {
            Ok(value) => assert!(value["allocator"].is_string()),
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::Unsupported),
        }
    }
}
