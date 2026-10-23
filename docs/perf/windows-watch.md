# Windows directory subscriptions

Windows subscriptions use `windows-file-watcher` 0.2.6, backed by native
thread-pool I/O. The notification queue's owned doorbell is registered with
`rt::Async`; the consumer waits for filesystem notifications or cancellation.
There is no timeout loop on a healthy subscription. Linux and macOS retain
`notify` and the existing parent-directory handling.

The dependency addresses idle wakeups: `notify` 8.2's Windows event loop wakes
every 100 ms even without filesystem activity. The
[8.2 source](https://docs.rs/crate/notify/8.2.0/source/src/windows.rs)
contains that timed wait. Native hosted observations include 15.625 ms CPU
increments in otherwise idle daemon windows. Size is report-only.

Registration waits for the monitor's submitted work before the consumer scans.
A root that disappears during registration returns an error. Detailed changes
retain native path names; queue overflow, coarse notification and identity
changes invalidate the root for a rescan. A subscription retains its monitor
and native reads through the async consumer's lifetime; dropping it signals
cancellation without requiring another filesystem event.

The maintained
[Windows backend](https://github.com/MikeGrier/windows-threadpool-sys/tree/windows-file-watcher-v0.2.6/crates/windows-file-watcher)
owns overlapped I/O, cancellation, path fidelity, overflow and fault recovery.
The adapter contains no Win32 calls or unsafe code.
