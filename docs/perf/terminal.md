# Terminal backend

The terminal backend uses the existing rustix/libc and windows-sys
bindings, with the existing vte decoder shared by every terminal. No new
dependency, periodic wake or input worker is added to the TUI. The Windows line
REPL restores console state directly in its control callback as well as on
normal return, a broken output pipe and panic; Unix cooked input needs no
additional waiter.

The maintained crossterm terminal library's
[documented raw-mode contract](https://docs.rs/crossterm/latest/crossterm/terminal/)
disables terminal-driver Ctrl-C processing. This client's Windows contract
instead retains processed input and receives Ctrl-C through the existing
SetConsoleCtrlHandler signal source. It also requires its VT byte decoder
and native resize records to share one wait with that signal source. Using
crossterm's high-level event decoder would replace that input contract;
using just its mode helpers would still need the console snapshot, code
pages, partial-setup rollback and control-event integration here. The
reliability contract, rather than artifact size, determines this choice.

Windows follows Microsoft's
[console modes](https://learn.microsoft.com/en-us/windows/console/setconsolemode)
and [stream plus event example](https://github.com/microsoft/terminal/tree/main/samples/ReadConsoleInputStream):
ReadConsoleInputW retains resize records while forwarding VT key characters
through UTF-16-to-UTF-8 conversion. ReadFile/ReadConsole alone discard native
resize events. Both console and signal handles participate in one infinite
WaitForMultipleObjects call. Console-free and redirected operation does not
require console mode changes.

Console output uses a dedicated synchronous writer, with one bounded pending
request. The renderer waits on completion and control events; on interruption it
calls CancelSynchronousIo for that writer only, retrying until the operation
acknowledges cancellation. A cleanup deadline bounds the caller's wait even
when a driver fails to acknowledge cancellation. The writer owns each buffer
until native completion, and an unresponsive writer closes the output path
instead of letting later frames spin on a full queue. The worker and renderer
have no periodic wake while idle. See Microsoft's
[synchronous I/O cancellation contract](https://learn.microsoft.com/en-us/windows/win32/fileio/canceling-pending-i-o-operations).

Unix output checks foreground ownership and blocks SIGTTOU only around
cleanup and mode restoration. Rendering lets the kernel report background
writes and rechecks ownership after interrupted calls. The renderer serializes
restoration and suspension. Fixed, coalescing control slots bypass the
bounded typing backlog; Ctrl-C invalidates already queued keystrokes.
An owned pause guard disables stdin while leaving signal delivery active
until ownership has been reacquired. Nonblocking output waits on readiness
and a dedicated control wake pipe; screen cleanup has a single deadline.
Rendering opens the terminal through its own nonblocking file description;
inherited stdout and the shell's shared file status flags are never changed.
The stop is directed
to the calling thread, which cannot re-enter raw mode before being continued.
There is no tcsetpgrp that could take the foreground from the shell.

Native tests use isolated pseudo-terminals or a private Windows console
subprocess. They cover suspension, redraw and continued input, panic mode
restoration, UTF-16 surrogate pairs, resize records, and native Ctrl-C and
Ctrl-Break callbacks. ConPTY tests pause native console writes while keeping
the frontend drained; they verify cancellation, the panic cleanup deadline,
mode/code-page restoration and zero idle worker CPU. Unix tests retain the
parent's inherited tty description through acquisition, suspension, resume,
normal exit and a killed child, checking that its flags never change.
Hosted CI is the authority for cross-platform
results. A PTY or console subprocess does not establish every emulator's
keyboard/mouse behavior; interactive Windows Terminal, tmux and SSH checks
remain separate from these automated invariants.
