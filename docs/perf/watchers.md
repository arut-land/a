# Source watching and preparation

Linux uses notify's inotify backend. macOS uses FSEvents for existing
source trees and kqueue vnode subscriptions for their parents. Windows
uses overlapped ReadDirectoryChangesW requests awaited through the runtime's
waitable-event reactor. Each request owns a DWORD-aligned 16 KiB buffer and
an OVERLAPPED allocation until completion or joined cancellation. There is
no notification thread or periodic wait on Windows.

WatchEvent carries changes, overflow, rescan requests and unavailable
subscriptions. Uncertainty invalidates all roots, including events without
paths; refresh rearms subscriptions before source inventory is captured.
Root parents remain watched so creation and replacement stay observable.

Lua resolves home/workspace precedence before opening winning sources.
Skills follows links only within configured roots and watches descendant
targets as well as SKILL.md entries. Source reads and pathname revalidation
use sys's bounded, nonblocking regular-file opens. Snapshot identity includes
native file identity, size, modification time and, on Unix, change time.
Bytes and failed reads are retained before UTF-8 decoding.

The I/O-free ext source driver captures inputs, waits an injected 100 ms
window, compares captures, compiles and revalidates, including failed
compilations. The host validates all layers again before publication. A
failed preparation retains the current registry and folded state. Empty
source sets skip the window. Settled sources leave no recurring timer.
File-follow opens, bounded reads and initial source-backed prompt context
use admitted Storage work before the kernel is stepped.

The window is a debounce, not a writer-completion signal. A writer paused
with valid partial content can outlast it. Native notifications are advisory;
no finite sequence of reads proves that an in-place writer has finished or
that a future change will generate a notification. An agreed inventory also cannot distinguish an intentional source removal
from a temporary rename gap. A generation requires its effective input set
to settle during capture; continuously changing effective inputs can delay
that generation. Atomic replacement or an explicit writer completion
protocol is required when the stronger publication guarantee matters. Permanent watch
or filesystem failure is reported; it cannot guarantee eventual reload.
