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
claims the record before acknowledging readiness, then starts accepting; clients wait for its hello.
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

## Invariants exercised

The portable black-box tests in
[`daemon_acceptance.rs`](../../apps/cli/tests/blackbox/daemon_acceptance.rs)
run on Unix and in the native Windows lane:

- `concurrent_starters_share_one_daemon_and_stop_releases_its_lock`
- `committed_turns_survive_a_daemon_restart`
- `an_unclean_exit_releases_the_endpoint_and_recovers_committed_sessions`
- `shell_paths_with_spaces_and_non_ascii_reach_the_native_directory`
- `two_installs_upgrade_once_and_the_older_client_never_downgrades`

Windows sys tests cover kernel-specific contracts:

- `alternate_spellings_of_one_home_find_the_same_pipe`
- `a_listener_has_one_owner_and_accepts_multiple_independent_clients`
- `a_cancelled_read_keeps_its_buffer_and_completion_until_observed`
- `shutdown_wakes_a_blocked_clone_without_waiting_for_the_peer`
- `the_pipe_dacl_grants_access_only_to_the_current_token_sid`
- `daemon_death_kills_the_whole_job_tree`
- `process_identity_is_stable_and_not_a_pid_alone`
- `extended_and_unc_paths_keep_their_shell_meaning`

Portable sys invariants include
`a_locked_record_remains_readable_and_excludes_other_writers` and
`shell_directory_reports_roundtrip_spaces_unicode_and_line_endings`;
both pass under Miri on Unix. The existing staging tests now run on Windows
as well. Client tests for stopped-process identity and releasing a timed-out
lock waiter also run on Windows through the shared lock seam.

## Local validation (2026-10-21)

On the Apple development host, the merged tree through main `0b9292f`
passed `mise run check` (944 tests, 28 skipped), acceptance (18), web (6),
and Apple (10). Windows cross-clippy, machete, dependency deny, and the
no-Tokio check passed. Native Windows linking and execution have not run
locally; the required CI lane is the gate for those results.

The two portable path/lock invariants above passed under Miri. The full
prescribed Miri task still stops at the existing unsupported
`CFStringCreateWithCString` call in `sys::awake`. Windows kernel FFI and
completion-buffer lifetimes cannot be checked by that Unix Miri run.

`mise run size` measured 3,007 KiB before and after the Windows work alone.
After merging main's fork implementation and protocol 7, it measured
3,039 KiB, including fixture exclusion, below the 4,096 KiB cap. All builds
used `CARGO_BUILD_JOBS=3`. Budgets and startup timing were deliberately not
run on this shared machine; the coordinator runs them on a quiet host.

## Explicit process inheritance

Daemon and shell creation now own three child stdio duplicates and use
`CreateProcessW` with an explicit `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
The shell's attribute list also assigns its kill-on-close job before user
code runs. No other inheritable handle in the parent is eligible. This
uses the existing windows-sys bindings; no package is added.

The pinned standard library duplicates stdio inside `Command::spawn`,
which prevents an external allowlist from naming the actual child stdio.
The sys seam therefore owns native creation and its child handle, while
Unix retains std::process::Child. The command carries an explicit environment
and argv; MS C argument escaping is shared with pure Unix/Miri tests.
The API takes the output file separately, eliminating hidden stdio state.

`a_cold_clients_output_reaches_eof_while_the_daemon_stays_alive` exercises
cold-start capture on every platform with a bounded EOF wait. Native
`a_shell_never_inherits_an_unrelated_inheritable_pipe` keeps a shell alive
while an unrelated inheritable pipe must reach EOF.

Pipe discovery uses the existing locked daemon record. The physical pipe name
includes the home identity, SID hash, PID and process creation time. Only the
lock holder publishes that record, before acknowledging startup; clients reject
stale process identities and verify the pipe server's PID and SID. Each
successor therefore creates its own namespace with FIRST_PIPE_INSTANCE even
when a client retains a previous generation's handle. No extra discovery file
or compatibility path is needed. The portable retained-client acceptance test
covers graceful restart; native CI exercises the Windows namespace lifetime.

## Durable namespace publication

Flushed files are published on NTFS with same-volume MoveFileExW using
REPLACE_EXISTING and WRITE_THROUGH. File content flush precedes the rename;
completion waits for the move on disk. No cross-volume copy fallback is
allowed. Imports, catalog rewrites and dirty markers use this path. A new
Windows log publishes its flushed header before handing out an append
handle and a Publication receipt; Unix retains its deferred name barrier.
Removing the catalog dirty marker durably renames it to an ignored tombstone
before deleting that tombstone. A crash can leave garbage, not a clean
catalog paired with an unpublished rewrite.

Directory FlushFileBuffers with BACKUP_SEMANTICS has no documented persistence
contract used here. Windows sync_dir returns Unsupported, rather than silently
certifying a name. The guarantee assumes local NTFS and devices honoring
flushes. Native CI exercises the operation and error paths; power-loss
persistence still requires a storage test. This changes no dependency and adds
one header flush/publication per new Windows session, without changing Unix's
shared first-entry barrier. No Windows performance measurement is claimed.

## Stop target ownership

Fallback stop opens one query+terminate process handle, validates the recorded
creation token through that handle, and keeps it across both stop attempts.
Termination never reopens the PID. Missing or mismatched Windows tokens are
rejected. The existing portable impostor-record test protects the public stop
path; the native handle test retains the verified target after exit, starts a
successor and confirms a second signal cannot affect that successor.

## Main integration

Main through b6a11da uses protocol 8 and adds a daemon signal listener and MCP
stdio. Windows console callbacks now signal a process-lifetime event registered
through rt; detached processes can wait without a console or a polling thread.
The permanently owned event avoids callback/handle-close reuse races.
MCP also uses the contained, explicitly allowlisted native spawn path. Its
parent stdio uses overlapped private pipes and the same completion ownership
as daemon transport; inherited child stdio remains synchronous. Unix keeps
ordinary readiness pipes. No extra package, worker thread or timer is added.
Native CI must execute these integration paths as well as the five fixes.
