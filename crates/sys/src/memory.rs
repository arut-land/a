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
