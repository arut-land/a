use windows_sys::Win32::System::{
    JobObjects::{
        JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        JOB_OBJECT_CPU_RATE_CONTROL_MIN_MAX_RATE,
        JOB_OBJECT_CPU_RATE_CONTROL_WEIGHT_BASED, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
        JobObjectCpuRateControlInformation, QueryInformationJobObject,
    },
    SystemInformation::{
        CpuSetInformation, GROUP_AFFINITY, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
    },
    Threading::{
        ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetCurrentProcess, GetCurrentThread,
        GetProcessAffinityMask, GetProcessDefaultCpuSets, GetThreadGroupAffinity,
    },
};

pub(super) fn parallelism(mut count: usize) -> usize {
    // SAFETY: process pseudo-handle is valid and all outputs are writable.
    unsafe {
        let process = GetCurrentProcess();
        let (mut affinity, mut system) = (0, 0);
        if GetProcessAffinityMask(process, &raw mut affinity, &raw mut system) != 0 && affinity != 0
        {
            count = count.min(affinity.count_ones() as usize);
        }
        if let Some(selected) = selected_cpus(affinity) {
            count = count.min(selected);
        }
        let mut rate = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
        if QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectCpuRateControlInformation,
            (&raw mut rate).cast(),
            u32::try_from(size_of_val(&rate)).expect("fixed-size job rate"),
            std::ptr::null_mut(),
        ) != 0
            && rate.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE != 0
            && rate.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_WEIGHT_BASED == 0
        {
            // A plain CpuRate without HARD_CAP is a soft target the scheduler
            // exceeds when the host is idle, so only a cap or maximum bounds.
            let maximum = if rate.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_MIN_MAX_RATE != 0 {
                Some(rate.Anonymous.Anonymous.MaxRate as usize)
            } else if rate.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP != 0 {
                Some(rate.Anonymous.CpuRate as usize)
            } else {
                None
            };
            // An immediate job rate bounds host capacity. Parent-job rates
            // require parent handles and are not exposed by this query.
            if let Some(maximum) = maximum {
                let host = GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) as usize;
                count = count.min((host * maximum / 10_000).max(1));
            }
        }
    }
    count.max(1)
}

fn selected_cpus(affinity: usize) -> Option<usize> {
    // The standard worker scheduler uses the calling thread's processor group.
    // Intersect CPU-set IDs with affinity; min(counts) can overcount overlapping masks.
    // SAFETY: every buffer is initialized, aligned and passed with its actual capacity.
    unsafe {
        let process = GetCurrentProcess();
        let mut required = 0;
        GetProcessDefaultCpuSets(process, std::ptr::null_mut(), 0, &raw mut required);
        if required == 0 {
            return None;
        }
        let mut ids = vec![0; required as usize];
        if GetProcessDefaultCpuSets(process, ids.as_mut_ptr(), required, &raw mut required) == 0 {
            return None;
        }
        ids.truncate(required as usize);
        let mut group = GROUP_AFFINITY::default();
        if GetThreadGroupAffinity(GetCurrentThread(), &raw mut group) == 0 {
            return None;
        }
        let mask = if affinity == 0 { group.Mask } else { affinity };
        let mut bytes = 0;
        GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &raw mut bytes, process, 0);
        if bytes == 0 {
            return None;
        }
        let mut buffer = vec![
            SYSTEM_CPU_SET_INFORMATION::default();
            (bytes as usize).div_ceil(size_of::<SYSTEM_CPU_SET_INFORMATION>())
        ];
        if GetSystemCpuSetInformation(buffer.as_mut_ptr(), bytes, &raw mut bytes, process, 0) == 0 {
            return None;
        }
        let mut offset = 0;
        let mut selected = 0;
        while offset + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
            let info = buffer
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<SYSTEM_CPU_SET_INFORMATION>()
                .read_unaligned();
            if (info.Size as usize) < size_of::<SYSTEM_CPU_SET_INFORMATION>() {
                return None;
            }
            if info.Type == CpuSetInformation {
                let cpu = info.Anonymous.CpuSet;
                let bit = 1usize
                    .checked_shl(u32::from(cpu.LogicalProcessorIndex))
                    .unwrap_or(0);
                if cpu.Group == group.Group && mask & bit != 0 && ids.contains(&cpu.Id) {
                    selected += 1;
                }
            }
            offset += info.Size as usize;
        }
        // Windows ignores a CPU-set assignment wholly excluded by hard affinity.
        (selected > 0).then_some(selected)
    }
}
