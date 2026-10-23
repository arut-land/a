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
        unsafe { windows_sys::Win32::System::SystemInformation::GetSystemInfo(&raw mut info) };
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
            .filter(|disk| {
                disk.mount_point()
                    .canonicalize()
                    .is_ok_and(|mount| path.starts_with(mount))
            })
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
#[must_use]
pub struct Priority {
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    #[cfg(target_os = "macos")]
    previous: (u32, i32),
}

impl Priority {
    pub fn user_initiated() -> Self {
        Self::enter(0x19)
    }

    pub fn utility() -> Self {
        Self::enter(0x11)
    }

    fn enter(class: u32) -> Self {
        #[cfg(target_os = "macos")]
        {
            let mut relative = 0;
            let mut previous = 0;
            // SAFETY: current thread is live; both output pointers are writable.
            unsafe {
                pthread_get_qos_class_np(libc::pthread_self(), &raw mut previous, &raw mut relative)
            };
            // SAFETY: class is USER_INITIATED or UTILITY; priority zero is valid.
            unsafe { pthread_set_qos_class_self_np(class, 0) };
            Self {
                previous: (previous, relative),
                _thread: std::marker::PhantomData,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = class;
            Self {
                _thread: std::marker::PhantomData,
            }
        }
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
    fn pthread_get_qos_class_np(
        thread: libc::pthread_t,
        class: *mut u32,
        relative: *mut i32,
    ) -> i32;
    fn pthread_set_qos_class_self_np(class: u32, relative: i32) -> i32;
}

#[cfg(windows)]
mod windows;

/// Event-driven pressure notification. The callback must only release disposable
/// state; cancellation can overlap a callback already in progress.
#[must_use]
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
                let callback = unsafe { Box::from_raw(context.cast::<Callback>()) };
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(callback)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore)]
    fn native_facts_are_nonzero_without_assuming_page_granularity() {
        let facts = Resources::for_path(&std::env::temp_dir()).unwrap();
        assert!(facts.parallelism > 0);
        assert!(facts.page_size.is_power_of_two());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[cfg_attr(miri, ignore)]
    fn affinity_restrictions_bound_effective_parallelism() {
        // A new thread inherits this test's affinity and exits with the restriction.
        std::thread::spawn(|| {
            // SAFETY: CPU sets are initialized and passed with their actual sizes.
            unsafe {
                let mut allowed: libc::cpu_set_t = std::mem::zeroed();
                assert_eq!(
                    libc::sched_getaffinity(0, size_of_val(&allowed), &raw mut allowed),
                    0
                );
                let first = (0..usize::try_from(libc::CPU_SETSIZE).expect("positive CPU set size"))
                    .find(|cpu| libc::CPU_ISSET(*cpu, &allowed))
                    .unwrap();
                libc::CPU_ZERO(&mut allowed);
                libc::CPU_SET(first, &mut allowed);
                assert_eq!(
                    libc::sched_setaffinity(0, size_of_val(&allowed), &raw const allowed),
                    0
                );
                assert_eq!(effective_parallelism(), 1);
            }
        })
        .join()
        .unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn nested_priority_scopes_restore_the_enclosing_work_class() {
        fn class() -> u32 {
            let mut class = 0;
            // SAFETY: current thread is live; class is writable, relative output is optional.
            let result = unsafe {
                pthread_get_qos_class_np(libc::pthread_self(), &raw mut class, std::ptr::null_mut())
            };
            assert_eq!(result, 0);
            class
        }
        let _utility = Priority::utility();
        {
            let _foreground = Priority::user_initiated();
            assert_eq!(class(), 0x19);
        }
        assert_eq!(class(), 0x11);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pressure_source_cancellation_releases_its_callback() {
        struct Released(std::sync::mpsc::Sender<()>);
        impl Drop for Released {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (sent, received) = std::sync::mpsc::channel();
        let released = Released(sent);
        let source = MemoryPressure::new(move || {
            std::hint::black_box(&released);
        });
        drop(source);
        received
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }

    #[cfg(windows)]
    fn check_cpu_sets(affinity: usize) {
        use windows_sys::Win32::System::{
            SystemInformation::{
                CpuSetInformation, GROUP_AFFINITY, GetSystemCpuSetInformation,
                SYSTEM_CPU_SET_INFORMATION,
            },
            Threading::{
                GetCurrentProcess, GetCurrentThread, GetThreadGroupAffinity,
                SetProcessDefaultCpuSets,
            },
        };
        // SAFETY: all native outputs are initialized and sized; this runs in the child.
        unsafe {
            let process = GetCurrentProcess();
            let mut bytes = 0;
            GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &raw mut bytes, process, 0);
            assert!(bytes > 0);
            let mut buffer = vec![
                SYSTEM_CPU_SET_INFORMATION::default();
                (bytes as usize).div_ceil(size_of::<SYSTEM_CPU_SET_INFORMATION>())
            ];
            assert_ne!(
                GetSystemCpuSetInformation(buffer.as_mut_ptr(), bytes, &raw mut bytes, process, 0),
                0
            );
            let mut group = GROUP_AFFINITY::default();
            assert_ne!(
                GetThreadGroupAffinity(GetCurrentThread(), &raw mut group),
                0
            );
            let id = buffer
                .iter()
                .find_map(|info| {
                    if info.Type != CpuSetInformation || info.Size == 0 {
                        return None;
                    }
                    let cpu = info.Anonymous.CpuSet;
                    let bit = 1usize
                        .checked_shl(u32::from(cpu.LogicalProcessorIndex))
                        .unwrap_or(0);
                    (cpu.Group == group.Group && affinity & bit != 0).then_some(cpu.Id)
                })
                .unwrap();
            assert_ne!(SetProcessDefaultCpuSets(process, &raw const id, 1), 0);
            assert_eq!(effective_parallelism(), 1);
            assert_ne!(SetProcessDefaultCpuSets(process, std::ptr::null(), 0), 0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_limits_are_observed_in_an_isolated_process() {
        use windows_sys::Win32::System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
                JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
                JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0, JobObjectCpuRateControlInformation,
                SetInformationJobObject,
            },
            Threading::{GetCurrentProcess, GetProcessAffinityMask, SetProcessAffinityMask},
        };
        if std::env::var_os("A_RESOURCE_LIMIT_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "resources::tests::windows_limits_are_observed_in_an_isolated_process",
                    "--nocapture",
                ])
                .env("A_RESOURCE_LIMIT_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        // The child alone changes process scheduling state, never the test runner.
        // SAFETY: outputs have their declared sizes, and the pseudo-handle is live.
        unsafe {
            let process = GetCurrentProcess();
            let (mut affinity, mut system) = (0usize, 0usize);
            assert_ne!(
                GetProcessAffinityMask(process, &raw mut affinity, &raw mut system),
                0
            );
            let one = 1usize << affinity.trailing_zeros();
            assert_ne!(SetProcessAffinityMask(process, one), 0);
            assert_eq!(effective_parallelism(), 1);
            assert_ne!(SetProcessAffinityMask(process, affinity), 0);
            check_cpu_sets(affinity);

            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null());
            let rate = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
                ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE
                    | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
                Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: 1000 },
            };
            assert_ne!(
                SetInformationJobObject(
                    job,
                    JobObjectCpuRateControlInformation,
                    (&raw const rate).cast(),
                    u32::try_from(size_of_val(&rate)).expect("fixed-size job rate")
                ),
                0
            );
            assert_ne!(AssignProcessToJobObject(job, process), 0);
            let host = std::thread::available_parallelism().map_or(1, usize::from);
            assert!(effective_parallelism() <= (host / 10).max(1));
            windows_sys::Win32::Foundation::CloseHandle(job);
        }
    }
}
