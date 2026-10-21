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
console errors. Existing crash reports under `crash/` remain separate and
may include panic text; this change does not claim to redact those reports.

Startup facts can enter the bounded queue before configuration is loaded.
The daemon installs one writer only after acquiring its home lock. A
producer tries a fixed 256-record queue and drops on contention or overflow.
It neither formats nor writes a file, waits for queue space, nor awaits a
log flush. A separate `rt` runtime drains batches through its blocking pool;
its stalled disk worker cannot occupy the session commit executor. The
queue and in-flight batch each hold at most 256 fixed-size records. Queue
losses and failed diagnostic writes are counted in a `lost` record when
writing succeeds again. Diagnostic I/O errors never recurse into logging.

Each file is capped at 256 KiB by rotating before every write, independent
of supervisor wakes. Live plus retained files use at most 768 KiB. Files
are reopened after rotation; a partial failed write is truncated before
another append is attempted. Reopening also truncates an unsealed tail left
by a previous process, so the first new fact is always a complete line. This is best-effort
observability, not a second durable ledger: it does not fsync. An abrupt
process death can lose queued facts or tear the final diagnostic line.
Orderly exit closes admission and gives queued writes at most 250 ms before
releasing the daemon lock. It never holds a session commit for diagnostics.

A daemon that cannot answer may be unable to report its own wedge. Local
clients leave their last typed timeout or forced-stop observation in
`run/daemon.witness`. It contains only the observed daemon pid/start stamp,
time and reason. It is one bounded record; concurrent observations can
replace or lose one another. The next daemon consumes it into the main log
and removes it. Clients never append or rotate the daemon log themselves.

The API has a compile-fail example rejecting `String`; deterministic tests
exercise arbitrary error content, stalled consumers, saturated queues,
write-failure accounting and rotation. The engine's Faulty-store test keeps
the diagnostic disk stalled through store recovery and a subsequent commit.
Black-box acceptance checks the crash recovery session and rule; a signal
exit test checks the recorded reason and absence of session content.
Dependency and size measurements are in [the logging probe](perf/logging.md).
