# Daemon diagnostics

`a daemon log` reads `$A_HOME/logs/daemon.jsonl` without connecting to or
starting a daemon. Each complete line is one JSON object with `unix_ms`,
`pid`, and a typed `fact`. `a daemon status` and a wedged client's diagnosis
also point to this file; the existing log-tail UI reads the same records.
The two retained files are `daemon.jsonl.1` and `daemon.jsonl.2`, newest first.

The session ledger remains the execution record. Diagnostics keep facts
that replay cannot reconstruct: the daemon build and start origin, clean
exit reason, a previous unexplained death and crash-breaker refusal,
upgrade destination, catalog rebuild reason, recovery rule, store fault
kind, torn-tail truncation or read-only preservation, protocol mismatch
range, provider failure classification, kernel panic, expired shutdown,
configuration fallback, Lua generation acceptance or rejection, and a
session's rejection of a prepared generation. A recovery record names the
full session UUID and either `resume_ledger` or `reload_after_fault`; tool
replay decisions and outcomes remain in the session ledger. Store repair
records give the chosen rule and byte offset. Interior damage is preserved,
not silently skipped.

`ext-logging` owns this diagnostic vocabulary and writer. This is process
infrastructure, independent of session folds and extension registration;
there are no logging decisions or settings in the kernel. Producers can
pass only sealed `Loggable` facts. Their fields contain enums, numbers,
UUIDs, and validated numeric build identities. No field admits `String`,
`str`, paths, payload JSON, error messages, provider bodies or request IDs.
Workspace and module identities use numeric fingerprints rather than text.
Build versions are three numeric components and the staged build's hex
fingerprint represented as a number. This is a diagnostic identity, not a
cryptographic anonymity guarantee.

There are no levels or `A_LOG` knob: every admitted fact is essential.
Inputs, outputs, successful calls, commits, session summaries and timing
traces belong elsewhere. The optional `engine/events` stream remains a
foreground debugging aid on stderr, not a second persistent transcript.
Detached stderr goes to the null device so arbitrary library diagnostics
cannot bypass the typed API. A manually launched daemon can still show
console errors.

Native delivery supplements the same readable local file. An explicit home
(`A_HOME` or `Home::from`) keeps diagnostics local and never opens a native
sink. The resolved home carries that distinction. Daemon spawn preserves an
explicit override and leaves `A_HOME` unset for a default per-user home. Otherwise Linux connects a nonblocking datagram socket
to `/run/systemd/journal/socket` and sends native journal entries with
`SYSLOG_IDENTIFIER=a`, priority notice and the sealed JSON record as `MESSAGE`.
A missing journal or a full socket never delays local delivery. Endpoint-loss
errors reconnect the same nonblocking socket to its pathname, then resend once.
Each record permits at most one reconnect and one resend, including across a
restart gap; a full collector never triggers reconnection. macOS uses
unified logging under subsystem `land.arut.a`, category `daemon`, at default
level with a static public string template. Windows uses the manifest-free
TraceLogging provider `land.arut.a.daemon`, event `Fact`, field `Json` at
informational level. Native enabled checks precede native string conversion;
the local record is encoded once regardless. Native transport has no worker,
retry queue, timer or idle wake. It is best effort, and its failures do not
contribute to the local file's write-failure count.

Crash reports under `crash/` use a separate closed JSON shape: validated
numeric build identity, pid and time, `panic` or `unclean_exit`, numeric
callsite fingerprint/line/column, and at most 128 frames. Each frame carries
an image fingerprint, image-relative offset and optional native identity:
Linux ELF GNU build-id, macOS Mach-O LC_UUID, or Windows PE timestamp,
image size and optional CodeView RSDS GUID bytes/age. Hex fields preserve
native byte order; the RSDS GUID's first three fields use Windows little-endian
GUID layout. Identities come from the loader's mapped image, so a moved,
replaced or deleted executable path cannot change the report's identity.
Linux resolves program counters directly through loaded ELF segments, including
static executables, and hashes the build-id for image correlation. Missing native
identity is explicit rather than substituted with a path.
The `object` crate parses bounded mapped ELF notes and Mach-O load commands;
compression and unused formats are disabled. Windows uses the existing
`windows-sys` PE definitions. No executable file is opened during a panic.
Windows captures frames with `RtlCaptureStackBackTrace` and pins each module
while copying its PE metadata through `ReadProcessMemory`. Discarded or
unreadable debug pages yield missing metadata; the debug scan is capped at
64 entries per frame. It does not initialize DbgHelp or symbolize.
Unix unwinds without symbolization. No panic payload, thread name, symbol,
image name, PDB filename or source path crosses the report boundary, including
debug builds and reaper reports. The panic hook replaces the default formatter;
it is not a fatal-signal handler. Each report is capped at 32 KiB and the newest
20 reports are retained (at most 640 KiB). A reaped unexplained exit has no
frames: the reaper must not attribute its own image identity to a dead daemon.

Startup facts can enter the bounded queue before configuration is loaded.
The daemon installs one writer only after acquiring its home lock. A
producer tries a fixed 256-record queue and drops on contention or overflow.
Admission and wake both avoid consumer-held locks: a coalescing atomic
flag and `rt::Wakeup` publish the wake even during waiter registration.
The producer neither formats nor writes a file, waits for queue space,
nor awaits a log flush. A separate `rt` runtime drains batches through its
blocking pool; its stalled disk worker cannot occupy the session commit
executor. The queue and in-flight batch each hold at most 256 fixed-size
records. Queue
losses and failed diagnostic writes are counted in a `lost` record when
writing succeeds again, including recovery within the final batch before
a flush. Diagnostic I/O errors never recurse into logging.

Each file is capped at 256 KiB by rotating before every write, independent
of supervisor wakes. Live plus retained files use at most 768 KiB. Files
are reopened after rotation; a partial failed write is truncated before
another append is attempted. Reopening also truncates an unsealed tail left
by a previous process, so the first new fact is always a complete line.
This is best-effort observability, not a second durable ledger: it does not
fsync. An abrupt process death can lose queued facts or tear the final diagnostic line.
Orderly exit closes admission and gives queued writes at most 250 ms before
releasing the daemon lock. It never holds a session commit for diagnostics.

A daemon that cannot answer may be unable to report its own wedge. Local
clients leave their last typed timeout or forced-stop observation in
`run/daemon.witness`. A status query records timeouts during both the hello
and the status reply. The witness contains only the observed daemon
pid/start stamp, time and reason. It is one bounded record; concurrent observations can
replace or lose one another. The next daemon consumes it into the main log
and removes it. Clients never append or rotate the daemon log themselves.

The API has a compile-fail example rejecting `String`; deterministic tests
exercise arbitrary error content, stalled consumers, saturated queues,
write-failure accounting and rotation. Crash tests use a multi-megabyte
panic payload and check the largest numeric report against its cap. The
engine's Faulty-store test keeps the diagnostic disk stalled through store recovery and a subsequent commit.
Black-box acceptance checks the crash recovery session and rule; a signal
exit test checks the recorded reason and absence of session content.
Dependency and size measurements are in [the logging probe](perf/logging.md)
and [the native diagnostics measurements](perf/native-diagnostics.md).
