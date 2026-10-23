//! Native facts for runtime admission. Query at startup, never on an idle timer.

use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Storage {
    Rotational,
    SolidState,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct Resources {
    pub parallelism: usize,
    pub page_size: usize,
    pub storage: Storage,
}

impl Resources {
    pub fn for_path(path: &Path) -> io::Result<Self> {
        Ok(Self {
            parallelism: effective_parallelism(),
            page_size: page_size()?,
            storage: storage(path),
        })
    }
}

/// Rust's Linux implementation intersects affinity with cgroup v1/v2 quota,
/// including ancestor limits and non-root controller mounts.
pub fn effective_parallelism() -> usize {
    let available = std::thread::available_parallelism().map_or(1, usize::from);
    #[cfg(windows)]
    return windows::parallelism(available);
    #[cfg(not(windows))]
    available
}

pub fn page_size() -> io::Result<usize> {
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no pointer arguments or side effects.
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(size)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(io::Error::last_os_error)
    }
    #[cfg(windows)]
    {
        let mut info = windows_sys::Win32::System::SystemInformation::SYSTEM_INFO::default();
        // SAFETY: info is a valid writable SYSTEM_INFO.
        unsafe { windows_sys::Win32::System::SystemInformation::GetSystemInfo(&mut info) };
        Ok(info.dwPageSize as usize)
    }
}

pub fn storage(path: &Path) -> Storage {
    let Some(path) = path.ancestors().find(|p| p.exists()) else {
        return Storage::Unknown;
    };
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = path.metadata() else {
            return Storage::Unknown;
        };
        let dev = meta.dev();
        let device = format!("/sys/dev/block/{}:{}", libc::major(dev), libc::minor(dev));
        let Ok(device) = std::fs::canonicalize(device) else {
            return Storage::Unknown;
        };
        // Partitions inherit their parent block device's queue. Device-mapper
        // and virtual devices may not reveal media; unknown stays explicit.
        for device in device.ancestors().take(2) {
            match std::fs::read_to_string(device.join("queue/rotational"))
                .as_deref()
                .map(str::trim)
            {
                Ok("1") => return Storage::Rotational,
                Ok("0") => return Storage::SolidState,
                _ => {}
            }
        }
        Storage::Unknown
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        let Ok(path) = path.canonicalize() else {
            return Storage::Unknown;
        };
        let disks = sysinfo::Disks::new_with_refreshed_list_specifics(
            sysinfo::DiskRefreshKind::nothing().with_kind(),
        );
        disks
            .iter()
            .filter(|disk| path.starts_with(disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().components().count())
            .map_or(Storage::Unknown, |disk| match disk.kind() {
                sysinfo::DiskKind::HDD => Storage::Rotational,
                sysinfo::DiskKind::SSD => Storage::SolidState,
                sysinfo::DiskKind::Unknown(_) => Storage::Unknown,
            })
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        let _ = path;
        Storage::Unknown
    }
}

/// Scoped priority for a blocking operation. Async workers keep their normal
/// priority because they multiplex both foreground and maintenance futures.
pub struct Priority {
    #[cfg(target_os = "macos")]
    previous: (u32, i32),
}

impl Priority {
    pub fn user_initiated() -> Self {
        #[cfg(target_os = "macos")]
        {
            let mut relative = 0;
            // SAFETY: pthread_self is live; relative is writable.
            let previous = unsafe { pthread_get_qos_class_np(libc::pthread_self(), &mut relative) };
            // SAFETY: USER_INITIATED is a valid qos_class_t; priority zero is valid.
            unsafe { pthread_set_qos_class_self_np(0x19, 0) };
            Self {
                previous: (previous, relative),
            }
        }
        #[cfg(not(target_os = "macos"))]
        Self {}
    }
}

impl Drop for Priority {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        {
            // UNSPECIFIED cannot be set explicitly; DEFAULT is its effective scope.
            let qos = if self.previous.0 == 0 {
                0x15
            } else {
                self.previous.0
            };
            // SAFETY: restoring a class/relative priority returned for this thread.
            unsafe { pthread_set_qos_class_self_np(qos, self.previous.1) };
        }
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_get_qos_class_np(thread: libc::pthread_t, relative: *mut i32) -> u32;
    fn pthread_set_qos_class_self_np(class: u32, relative: i32) -> i32;
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::System::{JobObjects::*, Threading::*};

    pub(super) fn parallelism(mut count: usize) -> usize {
        // SAFETY: process pseudo-handle is valid and all outputs are writable.
        unsafe {
            let process = GetCurrentProcess();
            let (mut affinity, mut system) = (0, 0);
            if GetProcessAffinityMask(process, &mut affinity, &mut system) != 0 && affinity != 0 {
                count = count.min(affinity.count_ones() as usize);
            }
            let mut required = 0;
            GetProcessDefaultCpuSets(process, std::ptr::null_mut(), 0, &mut required);
            if required > 0 {
                count = count.min(required as usize);
            }
            let mut rate = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
            if QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectCpuRateControlInformation,
                (&mut rate as *mut JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                std::mem::size_of_val(&rate) as u32,
                std::ptr::null_mut(),
            ) != 0
                && rate.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE != 0
                && rate.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_WEIGHT_BASED == 0
            {
                let maximum = if rate.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_MIN_MAX_RATE != 0 {
                    rate.Anonymous.Anonymous.MaxRate as usize
                } else {
                    rate.Anonymous.CpuRate as usize
                };
                // CPU rate is a fraction of host capacity, not the affinity subset.
                let host = std::thread::available_parallelism().map_or(1, usize::from);
                count = count.min((host * maximum / 10_000).max(1));
            }
        }
        count.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_facts_are_nonzero_without_assuming_page_granularity() {
        let facts = Resources::for_path(&std::env::temp_dir()).unwrap();
        assert!(facts.parallelism > 0);
        assert!(facts.page_size.is_power_of_two());
    }
}

/// Event-driven pressure notification. The callback must only release disposable
/// state; cancellation can overlap a callback already in progress.
pub struct MemoryPressure {
    #[cfg(target_os = "macos")]
    source: dispatch2::DispatchRetained<dispatch2::DispatchSource>,
}

impl MemoryPressure {
    pub fn new(callback: impl Fn() + Send + Sync + 'static) -> Self {
        #[cfg(target_os = "macos")]
        {
            use dispatch2::{DispatchObject, DispatchSource};
            type Callback = Box<dyn Fn() + Send + Sync>;
            extern "C" fn event(context: *mut std::ffi::c_void) {
                // SAFETY: source owns this box until finalization, after callbacks.
                let callback = unsafe { &*context.cast::<Callback>() };
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback));
            }
            extern "C" fn destroy(context: *mut std::ffi::c_void) {
                // SAFETY: finalizer runs exactly once after callbacks have ended.
                drop(unsafe { Box::from_raw(context.cast::<Callback>()) });
            }
            let callback: Box<Callback> = Box::new(Box::new(callback));
            // SAFETY: memory pressure uses no handle; WARN|CRITICAL is valid.
            let source = unsafe {
                DispatchSource::new(
                    std::ptr::addr_of!(dispatch2::_dispatch_source_type_memorypressure).cast_mut(),
                    0,
                    2 | 4,
                    None,
                )
            };
            // SAFETY: source owns context; only the finalizer frees it.
            unsafe {
                source.set_context(Box::into_raw(callback).cast());
                source.set_finalizer_f(destroy);
            }
            source.set_event_handler_f(event);
            source.activate();
            Self { source }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = callback;
            Self {}
        }
    }
}

impl Drop for MemoryPressure {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        self.source.cancel();
    }
}
