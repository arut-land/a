# Windows daemon platform

The transport uses byte-mode local named pipes. The name includes hashes of
both the home directory's file identity and the current process token's
user SID, so case variants and junction aliases find the same daemon. Every
instance has an explicit protected DACL containing one allow ACE for that SID, rejects
remote clients, and the first instance refuses an existing pipe name.
Clients query the server process's token SID before writing a hello. The
name's hashes select an endpoint; they do not authorize access.

`async-io` 2.6.0 provides Windows socket polling and `Waitable` handles. Its
waitable implementation uses the Windows thread pool, alongside polling's
IOCP socket reactor. It does not submit named-pipe operations. `sys` submits
`ReadFile`, `WriteFile` and `ConnectNamedPipe` with overlapped I/O and one
manual-reset event per direction. `rt` registers duplicate event handles
with `Waitable`; no blocking worker, timer, or dedicated thread waits for
an idle connection. The operation owns its pinned OVERLAPPED and buffer,
including across cancelled futures. Dropping an operation cancels it and
waits for completion before freeing either allocation.

The only added external dependency is a direct Windows-only use of
`windows-sys` 0.61.2, already present with `windows-link` 0.2.1 in the locked
Windows graph. Cargo.lock adds no package or version. These bindings avoid
hand-maintaining Win32 ABI layouts, constants and imports. `interprocess`
2.x supplies synchronous pipes but its asynchronous named pipes require
Tokio, which this binary cannot link. Reusing async-io's waitable support
avoids a second executor or reactor. No Windows size, RSS or latency
measurement is possible on the local Apple host; native measurements remain
an owner/CI follow-up, not evidence for a performance claim.

Windows daemon startup uses a re-listen handshake under `start.lock`.
After checking the daemon lock, the starter closes its record handle,
launches the child with `DETACHED_PROCESS`, and waits at most ten seconds for the child's binding acknowledgement through its
stdout pipe. No daemon transport or lock handle is inherited. The child
then claims the record and starts accepting; clients wait for its hello.
Unix retains the bound-descriptor handoff and backlog connection.

Record locks cover one byte far beyond the record's contents. Windows
byte locks would otherwise prevent clients reading the identity that
justifies stopping a wedged daemon. Start time comes from `GetProcessTimes`;
stop verifies that token before terminating a nonresponding process.

Windows staging uses atomic reference files for `current` and stamps,
rather than symlinks requiring developer mode or elevation. Staged builds
are executable `.exe` files. A running executable that Windows refuses to
remove is left for a later prune. The existing upgrade lifecycle drains
the old daemon, whose successor re-listens after the old process exits.

Shell execution uses Git for Windows Bash with redirected output and no
console. Native paths, extended-length/UNC prefixes, shell working-directory
reports and Windows daemon environment variables are handled in `sys`.
Directory-change notifications wake output followers without periodic
file polling. Each shell enters an unnamed kill-on-close job through the
process creation job-list attribute, before any shell code can create descendants. Only the
daemon retains the job handle, so daemon death kills the whole tree. There
is no terminal/ConPTY: shell tools have no interactive stdin, as on Unix.

Native CI runs workspace nextest and the portable daemon acceptance tests
without an advisory failure allowance. It alone can validate Win32 linking,
DACL contents, pipe completion/cancellation, handle wakeups, record locking,
no-console detach, job containment, Windows staging/upgrade, crash recovery,
and shell execution in paths with spaces and non-ASCII characters. A second
account denial test and physical power-cut durability testing need a suitable
Windows host; ordinary CI and cross-lint cannot prove those. Miri on Unix
cannot execute Windows FFI or validate kernel buffer lifetimes. Portable
path/staging/locking tests run locally; supported sys tests run under Miri.
