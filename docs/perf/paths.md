# Directory resolution measurements

Windows directory resolution uses the shell-folders and COM API contracts for
`SHGetKnownFolderPath` and its matching `CoTaskMemFree`. It loads them only for
native resolution, using a System32-only search, copies the returned path,
frees the allocation, and releases both loader references. Isolated `A_HOME`
resolution opens neither contract. Current-user Known Folder redirection and
UTF-16 paths remain the API's responsibility.

Microsoft's [API metadata](https://github.com/MicrosoftDocs/sdk-api/blob/docs/sdk-api-src/content/shlobj_core/nf-shlobj_core-shgetknownfolderpath.md)
lists the shell-folders contract alongside the Shell32 entry point. Hosted x64
and ARM64 probes found no direct KernelBase export on their Windows images;
the contract returned LocalAppData successfully on both. The Windows bindings
link `CoTaskMemFree` from Combase, rather than loading a separate Ole32 module.

`libloading` 0.9 owns balanced loader references and symbol lookup errors instead
of duplicating that lifecycle with raw loader handles. It adds one package on
Windows and depends only on the already-used `windows-link` there. The
Linux/macOS dependency graphs do not include it. Binary size is reported with
the paired measurements.

## Loaded daemon modules

The [complete module lists](paths-modules.json) retain the source revisions and
hosted run IDs for main, the static-import implementation, and the scoped-loader
implementation. Each release fixture daemon uses a fresh isolated `A_HOME` and
is stopped through that home. These working-set snapshots follow `a ls` and
three idle seconds; they establish residency, rather than a timing distribution.
The comparison main for these captures is `4d978b0b`; each record retains its
full source revision.

| Target | Original main | Static imports | Main at capture | Scoped loader |
| --- | ---: | ---: | ---: | ---: |
| Windows x64 RSS (KiB) | 8,040 | 10,276 | 8,100 | 8,052 |
| Windows ARM64 RSS (KiB) | 8,888 | 11,120 | 8,960 | 8,860 |

The static imports add `combase.dll`, `shell32.dll`, `gdi32.dll`,
`gdi32full.dll`, `imm32.dll`, `msvcp_win.dll`, `user32.dll`, and `win32u.dll`
on both architectures, plus `wintypes.dll` on x64. None of those additions
remain in the scoped-loader daemon. Main also maps `userenv.dll`, which the
scoped-loader implementation does not retain.

The fresh-process invariant
`sys::paths::native::tests::native_paths_do_not_retain_shell_or_com_modules`
actually calls native resolution and checks that Shell32, Ole32 and Combase
are absent afterwards. This verifies native lookup as well as the isolated
benchmark path.

## Windows cold reply

Five-round ARM64 reruns gave cold-reply median differences of -0.754 ms,
+1.048 ms and +2.845 ms. The last result warranted a larger measurement rather
than attributing it to sampling variance. The
[50-round probe](https://github.com/arut-land/a/actions/runs/40045489577)
compares main `4b662d7a` with the same implementation, alternates execution
order, and uses a fresh isolated home for each observation. Both binaries
first stage their executable and stop their daemon, matching the standard
cold-reply workload. [All observations](paths-windows-cold.json) are retained.

| Windows ARM64 measurement | Main | Candidate |
| --- | ---: | ---: |
| Cold-reply median | 66.012 ms | 66.116 ms |
| In-process isolated path resolution median | 0.0078 ms | 0.0188 ms |

The cold-reply difference between role medians is +0.104 ms. The median of
the 50 within-round differences is -1.026 ms, with a fixed-seed, 10,000-resample
percentile bootstrap 95% interval of [-1.747, +0.636] ms. This larger run does
not establish a positive cold-reply regression. The resolver's +0.011 ms
cost does not explain the earlier +2.845 ms five-round result. The isolated
path does no Known Folder lookup; native resolution is covered by the
separate fresh-process unload invariant. These measurements support a
near-zero residual cost, without claiming exact equality on hosted machines.

## Early-exit test synchronization

The hosted macOS repetition in
[run 40027460048](https://github.com/arut-land/a/actions/runs/40027460048)
reproduced `ext-shell::process::tests::an_exited_watch_waits_for_reapable_status`:
main failed 2 of 100 runs at `process_started(id).is_some()`. A killed,
unreaped child need not remain visible through Darwin's process-info query.
That assertion conflated process-table visibility with status acquisition.

The test controls the deferred status task directly: the launch future must
remain pending with one queued acquisition task, then succeed after that task
runs. It checks failed exit status, repeated consumers of the cached status,
and release of process ownership. The repaired test passed 500 of 500 hosted
macOS repetitions. The kernel's waitable-child/lease invariant remains covered
separately by `a_waited_leader_remains_our_child_until_its_lease_is_released` in
`sys::proc_unix`.

## macOS first lookup

IPC uses the endpoint backend's private `/tmp/a-UID` runtime result. Native
application temporary files still use `confstr(_CS_DARWIN_USER_TEMP_DIR)`.
An isolated `A_HOME` does not need Darwin temporary-file discovery to select
its transport address.

The [first-lookup profile](paths-lookup.json), from
[run 40038232397](https://github.com/arut-land/a/actions/runs/40038232397),
uses 100 fresh processes per mode and alternates their order. A native monotonic
clock times the two-call `confstr` lookup, allocation and result consumption
inside each process; process launch is outside the interval. The control
constructs and consumes the private UID runtime path. All samples are retained.

| Platform | Private runtime median | Darwin temp median |
| --- | ---: | ---: |
| macOS Intel | 0.009 ms | 2.893 ms |
| macOS ARM64 | 0.002 ms | 1.672 ms |

This identifies avoidable first-lookup work, independently of process-launch
variance. It does not assign the entire end-to-end cold-reply difference to
that lookup. In the unchanged-IPC paired rerun
[40027467738](https://github.com/arut-land/a/actions/runs/40027467738),
Intel cold reply changed by -8.692 ms, so the earlier +3.6 ms aggregate did
not reproduce. ARM attach p99 changed by +0.056 ms, so its earlier +0.878 ms
tail increase did not reproduce either. Each paired run has five alternating
reference/candidate rounds; each attach observation replays all expected log
entries across 500 connections.

## Paired integration measurements

[Perf 40040797566](https://github.com/arut-land/a/actions/runs/40040797566)
passes on all six native targets. Each lane compares implementation `8e7b346a`
against merged main `4b662d7a` in five alternating rounds on the same hosted
runner. Positive differences mean more bytes, memory or time. Sizes are the
shipped binary without fixtures; runtime observations use the same fixture
workload on both revisions. The [complete median deltas](paths-paired.csv)
include startup CPU, driver and decoder workloads as well as these metrics.

| Target | Shipped bytes | Size delta (KiB) | Cold reply delta (ms) | Idle RSS delta (KiB) | Attach p99 delta (ms) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Linux x64 | 4,300,232 | +8.625 | +0.551 | +60 | -0.005 |
| Linux ARM64 | 3,697,832 | +7.625 | -0.023 | +20 | -0.007 |
| macOS Intel | 3,998,680 | +8.039 | +4.775 | +48 | +0.216 |
| macOS ARM64 | 3,330,272 | +16.188 | +1.270 | -128 | -0.048 |
| Windows x64 | 5,549,056 | +11.000 | -1.975 | -12 | -0.018 |
| Windows ARM64 | 4,882,432 | +13.500 | +2.845 | -88 | -0.001 |

The table retains the five-round Windows ARM64 cold result; the larger
50-round cold-only run above puts that observation in context. Windows x64
cold differences in the two earlier paired runs were +0.537 and -0.143 ms;
their idle RSS differences were 0 and -36 KiB. Windows ARM64's earlier idle
RSS differences were -112 and -116 KiB. None reproduce the static imports'
roughly 2 MiB residency increase.

All idle client CPU samples are zero. All daemon samples are zero except one
15.625 ms accounting tick on Windows ARM64 in each role; idle CPU medians are
zero on every target. Directory resolution installs no timer, worker or
background retry. The full-run distributions and per-round observations are
in the linked hosted artifacts.

The latest macOS ARM64 attach p99 difference is -0.048 ms, following +0.056
and -0.482 ms in the earlier paired runs. Its original +0.878 ms increase
does not recur. Intel cold differences after restoring the endpoint runtime
policy are +2.554 and +4.775 ms in five-round runs; those observations require
the larger cold-only sample rather than a claim that the lookup correction
explains every end-to-end difference.

## Validation

[CI 40040794511](https://github.com/arut-land/a/actions/runs/40040794511)
passes on implementation revision `8e7b346a`, including Linux and macOS
checks and budgets, native Windows x64/ARM64 checks and acceptance, the Apple
tests/build, Miri, fuzzing, dependency checks and the no-Tokio lint.
That revision merges main `4b662d7a`, including the shell, maintained-crates
and power changes. Later measurement tooling and reports do not change
production code.

Path invariants cover isolated storage staying below its root, missing runtime
directories falling back to a private directory without logind, relative XDG
values never depending on cwd, writable state independent of a read-only home,
daemon environment preservation, separate macOS durable/cache locations, and
Windows credentials and machine state remaining local. IPC continues to use
the endpoint backend's runtime result and its existing transport invariants.
Executable staging policy remains a separate change.
