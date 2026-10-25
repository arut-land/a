# Workspace index (2026-10-25)

`ext-index` is a pruned copy of FFF's search core (docs/ext-index.md).
AGENTS.md asks for a measured reason before taking it: this compares FFF
as it ships, the `nucleo-matcher` + `ignore` + `grep-searcher` stack, and
the pruned port, on the same tree.

## Setup

Apple M3, eight cores, macOS 27.0, nightly-2026-10-17. Each arm is one
probe binary (`fff-research/probe` in the session scratchpad), built with
the repository's release profile (`opt-level = "z"`, fat LTO, stripped).
The tree is Linux v6.12 (shallow clone): 86,602 files after `.gitignore`,
1.4 GB, a git working tree, warm in the page cache. Load average 1.8–2.2
while measuring; three runs per arm, figures are the median. Footprint is
`footprint`'s `phys_footprint` two seconds after the work, RSS `ps -o rss`.

- **FFF**: `fff-search` 0.11.0 (7353971) unchanged: `FilePicker` with its
  scan, git status, frecency, watcher and global rayon pools (17 threads).
- **nucleo stack**: `ignore` parallel walk into one path arena,
  `nucleo-matcher` 0.3 `Pattern::score` over every path on one thread,
  `grep-searcher` + `grep-regex` over every file on as many threads as
  cores (there is no content index to consult).
- **Port**: `ext-index` through its public API on `rt::Threads` (four async
  workers, the default blocking pools).

## Results

| | FFF | nucleo stack | Port |
| --- | ---: | ---: | ---: |
| Walk to a searchable list | 240 ms | 110 ms | 185 ms |
| Content index build | 1.6 s | none | 2.0 s |
| Footprint, file list only | 89 MB | 12 MB | 28 MB |
| Footprint, with content index | 113 MB | (12 MB) | 72 MB |
| Per 100k files, list only (less the empty-root process) | 99 MB | 12 MB | 28 MB |
| Per 100k files, list and content index | 127 MB | – | 79 MB |
| Release binary, over the empty probe | +2.7 MiB | +1.2 MiB | – |
| `a`, with the `index` feature over without | – | – | +1.30 MiB |

Fuzzy search, warm, top 20 (ms):

| Query | FFF (all cores) | nucleo (one thread) | Port (CPU pool, ≤4 parts) |
| --- | ---: | ---: | ---: |
| `main.c` (56k matches) | 3.9 | 5.2 | 5.0 |
| `kconfig` | 2.7 | 3.0 | 3.7 |
| `drvnetintel` | 3.0 | 5.0 | 4.4 |
| `mm/slab` | 1.9 | 4.1 | 3.5 |
| `ext4inode` | 1.3 | 3.5 | 1.9 |
| `zstddecomp` | 0.9 | 2.3 | 1.5 |

The port's ranking is FFF's: the same first result for every query
(`Kconfig` for `kconfig`, `lib/zstd/zstd_decompress_module.c` for
`zstddecomp`). Ties go to the more recently modified file, so a fresh
clone, every file the same age, can break a tie (`main.c`) differently. nucleo matches far fewer files (no typo tolerance: 2,953
for `main.c` against 56,467) and ranks differently.

Grep, whole tree, every match (ms):

| Pattern | FFF | nucleo stack | Port |
| --- | ---: | ---: | ---: |
| `EXPORT_SYMBOL_GPL` (20,180 matches, 5.8k files read) | 116 | 1,440 | 109 |
| `zstd_decompress_stream` (17 matches, ~320 files read) | 11 | 1,450 | 22 |
| absent string (~100 files read) | 8 | 1,440 | 15 |

## Reading it

The content index is what FFF brings that no crate stack does: a grep
reads 0.1–7 % of the tree, 13–100x faster than reading every file the way
ripgrep does. Building it reads the tree once (2 s here); the first grep
pays that, later ones the figures above. Without it, the port greps as
the nucleo stack does (the degraded mode above the memory limit).

Fuzzy ranking is FFF's typo-tolerant Smith-Waterman (`neo_frizbee`) with
its filename and path bonuses; nucleo would need its own ranking work to
match it. Scored in up to four parts on the CPU pool, the port is within
a millisecond or two of FFF's all-core scoring and ahead of one nucleo
thread.

The port's memory is a third of FFF's for the list and two thirds with
the content index: FFF's git status, frecency databases and per-file
fields are gone, and paths live in one arena. The nucleo stack's list is
smaller still (12 MB) because it keeps paths alone, not the size and
time grep and ranking use.

## Choices measured on the way

- **Walk**: one `ignore` walker thread took 560 ms, one blocking job per
  top-level directory 380 ms (`drivers/` dominates), `ignore`'s parallel
  walker 165–200 ms with eight threads, 180 ms with four (the daemon's
  storage lane on this machine, which is what it now takes: the CPUs the
  process may use, halved, at most four). Its threads are scoped to the
  walk, inside one blocking job; no pool outlives it.
- **Content build**: jobs awaited in order ran one at a time (14 s): a
  blocking job is admitted when its future is first polled. Spawned, so
  four run at once: 2.0–2.3 s.
- **Builder memory**: one allocation per bigram column left 148 MB
  resident; one allocator slab per builder, 165 MB. The resident memory
  was freed large blocks in macOS's deferred-reclaim cache ("Malloc Large
  (empty)", 125 MB), which `malloc_zone_pressure_relief` did not return.
  Mapped from the OS (`sys::ZeroedWords`, `mmap`/`VirtualAlloc`) and
  unmapped after compression: 72 MB.
- **Opt level**: `opt-level = 3` for `ext-index`, `neo_frizbee`,
  `regex-automata`, `memchr` and `aho-corasick` made single-threaded fuzzy
  search about 30 % faster and grep unchanged; not taken, the parts on the
  CPU pool gave more.
- **Regex Unicode tables**: patterns are matched as bytes with ASCII
  classes, so `regex`'s and `regex-syntax`'s `unicode` features are off:
  −355 KiB of `a`.

The `index` feature adds 1.30 MiB to `a` (3.53 → 4.83 MiB, macOS arm64):
`regex`'s engines, `ignore` and `globset`, `aho-corasick`, `neo_frizbee`
and the port. No index is open in the idle RSS scenario (nothing asks),
so the idle budget does not move.

## Eager lifetime and persistence (2026-10-26)

Measured on the same Apple M3 (16 GiB), macOS 27.0.1, using the Linux
v6.12 release tarball in a temporary directory with an empty `.git`
directory so the checked-in ignore rules apply: **86,602 files**. A
standalone probe calls the real `Indexes`/`WorkspaceIndex` APIs over
`rt::Threads::new(4)`, with the repository's size-oriented release settings
(`opt-level = "z"`, fat LTO, one codegen unit). The probe gives content
building 1 GiB, as in the earlier large-tree comparison; it uses a ten-second
linger to observe held and resting states. Production defaults remain a
128 MiB content-build budget and now a thirty-second linger.

Three independent processes, separate initially empty caches, no deliberate
page-cache eviction. Other sessions share this development host; load
average was around four to six. The first run was slower (423 ms walk,
7.22 s build); all runs are included below. These are local measurements
on the Linux source tree, not measurements on a Linux host. Raw output,
probe source, phase CPU/RSS samples and `footprint` reports are retained
outside the worktree in `/private/tmp/index-eager-measure`.

| Operation | Median | Range (three runs) |
| --- | ---: | ---: |
| Cold walk to searchable files | 218 ms | 201–424 ms |
| Content build after the walk | 2.34 s | 2.30–7.22 s |
| Same-daemon reload and first grep | 45.8 ms | 45.6–46.0 ms |
| New-daemon snapshot read and first search | 42.2 ms | 41.1–49.5 ms |
| New-daemon reload through verified search results | 337 ms | 296–403 ms |
| CPU, same-daemon reload and first grep | 45.8 ms | 45.6–45.9 ms |
| CPU, new-daemon reload and verification workload | 1.59 s | 1.45–1.62 s |
| Raw cache reads into ordinary buffers, after verification | 4.21 ms | 4.15–4.69 ms |
| CRC-32C over cached bytes, ten iterations per run | 8.00 ms | 7.71–9.33 ms |

The same-daemon measurement includes decoding, CRC validation and the first
grep. The new-daemon probe creates one file after the old store is dropped,
then searches until that file appears; its CPU figure includes repeated
fuzzy searches as well as the background verify walk and change comparison.
Verification does not reread unchanged source contents. Initial searches
can use the snapshot while that verification runs. The earlier cold
baseline was 185 ms walk plus 2.0 s build; the repeated current measurements
are the closer comparison. Same-daemon reload avoids both operations.

The cache is **37,229,401 bytes (35.50 MiB)**, file list and bigram columns
combined. The live list/content heap accounting is **37,836,849 bytes**,
falling to zero at rest; subscriptions and ignore rules remain. Persistence
used 33–63 ms of process CPU; its wall time was observed behind the linger
deadline and a ten-millisecond sampling interval, so is not reported as a
separate write-latency measurement.

| Memory | Held | Resting |
| --- | ---: | ---: |
| RSS, median | 66.95 MiB | 46.70 MiB |
| RSS, range | 65.88–75.86 MiB | 45.62–52.38 MiB |
| `footprint` physical footprint, median | 62 MB | 42 MB |

Dropping ordinary `Vec` columns did not reliably reduce RSS: one initial
process went from 70.3 MB held to 73.7 MB resting, with macOS retaining freed
large allocations. Dense retained columns now use the existing
`sys::ZeroedWords` anonymous mappings, including when decoded directly from
disk. Dropping those returns their pages immediately; mapping refusal uses
ordinary allocations so queries still work. File-list and smaller build
allocations can remain in the allocator, and retained watcher/rule state
also contributes to the resting process footprint. Resting is a reduction,
not a promise to return a previously busy process to its startup RSS.

**File-backed mmap is not taken.** Streaming load, validation and first
query cost about 46 ms for 35.5 MiB; raw reads cost four milliseconds and
CRC eight. A mapped format would still need validation and an implementation
of the owned structures, would need an additional unsafe seam, and would
complicate truncation and replacement (SIGBUS on Unix, replacement of
mapped files on Windows). Anonymous owned mappings solve the observed
allocator-retention problem without mapping cache files.

**Linger is thirty seconds rather than five minutes.** Reload is about
56 times cheaper than this run's median cold walk plus build. Thirty
seconds retains a working index between nearby requests and replies,
while releasing its large structures ten times sooner. A session's
subsequent model replies renew the deadline. No periodic cleanup or idle
polling was added: the retained regression tests observe zero runtime
wake-ups for both quiet active and resting indexes.
