# Terminal backend

The terminal backend uses the existing rustix/libc and windows-sys
bindings, with the existing vte decoder shared by every terminal. No new
dependency, periodic wake or input worker is added to the TUI. The Windows line
REPL restores console state directly in its control callback as well as on
normal return, a broken output pipe and panic; Unix cooked input needs no
additional waiter.

Crossterm was considered as the maintained terminal implementation. Its
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

Unix output checks foreground ownership and blocks SIGTTOU only around
cleanup or an owned write, preventing restarted writes from trapping the
renderer after the shell changes foreground jobs. The renderer serializes
restoration and suspension; an acknowledged suspend event parks the input
reader until ownership has been reacquired. There is no tcsetpgrp that
could take the foreground from the shell.

Native tests use isolated pseudo-terminals or a private Windows console
subprocess. They cover suspension, redraw and continued input, panic mode
restoration, UTF-16 surrogate pairs, resize records, and native Ctrl-C and
Ctrl-Break callbacks. Hosted CI is the authority for cross-platform
results. A PTY or console subprocess does not establish every emulator's
keyboard/mouse behavior; interactive Windows Terminal, tmux and SSH checks
remain separate from these automated invariants.
