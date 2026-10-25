# Workspace index

`index` is a default CLI feature and an extension (`crates/ext-index`),
disabled with `[ext] disable = ["index"]` or left out of a build without
the `index` feature. It keeps one file index per workspace in the daemon,
shared by every client and session working there, and answers from it:

| Name | Kind | What |
| --- | --- | --- |
| `files.search` | workspace query, at once | The best files for what is typed: an @-mention completing |
| `files.grep` | workspace query, streamed | The lines that match, one update per file, then a summary |
| `glob` | model tool (`search`) | The files a glob matches, most recently modified first |
| `grep` | model tool (`search`) | The lines that match, as ripgrep prints them |

The tools are a second extension, `search` (`crates/ext-search`, the
`search` feature, on by default), over the indexes `index` provides: it
is dropped with `[ext] disable = ["search"]` and keeps the index and its
queries; without `index` it is refused.

A query about a session answers about the session's workspace; a client
can ask about a workspace no session has started in, as an @-mention
before the first message does.

## Requests and replies

`files.search` takes `{query, limit = 20}`. The query is fuzzy text with
FFF's constraints: `*.rs` (extension), `src/` or `/src/` (path segment),
`!test` (exclusion), globs with a `/` or braces (`src/**/*.rs`,
`{lib,src}`), several words (each must match). Typos are tolerated (two
for short queries, up to six for long ones). It replies `{files: [{path,
ranges}], matched, total}`: relative `/`-separated paths, best first, with
the byte ranges that matched for highlighting. A query of constraints alone
lists the files that pass them, most recently modified first.

`files.grep` takes `{query, mode = "plain" | "regex" | "fuzzy", case =
"smart" | "sensitive" | "insensitive", context = 0, limit = 500}`, with
the same constraints in `query`. Each update is `{path, matches: [{line,
text, ranges, before, after}]}`; the reply is `{searched, files, matches,
truncated, regex_error}`. `\n` in a pattern searches across lines. A regex
that does not compile is searched as literal text, and `regex_error` says
why. A constrained search that finds nothing is tried again as the whole
raw text, since constraint parsing can take tokens meant literally (`!=`).
Cancelling (or hanging up) stops it between chunks of files; the reads in
flight stop at their next file.

`glob` takes `{pattern}`: with a `/` it matches whole relative paths
(`src/**/*.rs`), without one, file names (`*.toml`); at most 500 are listed.
`grep` takes `{pattern, glob?, regex?, context?}`: plain text unless
`regex`, case-insensitive unless the pattern has a capital, at most 200
matches.

## What is indexed

The files a ripgrep walk of the workspace finds: inside a git working
tree (as `sys` finds one: `.git` files, `GIT_DIR`, `GIT_WORK_TREE`,
ceilings), what
`.gitignore`, `.ignore`, `.git/info/exclude` and the global excludes leave,
hidden files included, `.git` itself excluded; outside git, hidden files
and the usual tool and cache trees (`node_modules`, `target/debug`,
`.venv`, …) are left out too. Symbolic links are not followed. Paths that
are not UTF-8 are left out.

## Lifetime

The first query or call about a workspace opens its index: the walk runs
on the runtime's blocking pool (ripgrep's parallel walker, its threads
scoped to the walk), with the watch armed first so nothing that
changes during it is missed. The walk's threads are as many as the
daemon's storage lane has workers. Changes are applied as they arrive: a
changed file is looked at again, a changed directory walked again, a
removed one dropped, all by bisection of the sorted file list. A change
to an ignore file or a watcher overflow or rescan walks the whole root
again, and so does the root going, coming back or being replaced at its
path by another, which the watcher tells.

The index subscribes to the daemon's watcher (`rt::watch::Hub`, see
[watchers](perf/watchers.md)) with its root entered and `.git` left out,
and enters each directory its walk descends into, and each one a change
brings: on Linux only those take an inotify watch, so an ignored
`target/` or `node_modules` takes none. A second subscription follows
`.git/info`, whose `exclude` changes what the walk finds. Each walk
enters into a new subscription, which replaces the last once the walk is
installed; the hub follows the root itself, and its going, return or
replacement is a full walk.

The index stays open for five minutes after its last use, held by one
sleeping task, then closes with its watcher. A daemon nobody searches
holds no index: it is never in the idle RSS scenario. An open index that
nothing changes or asks costs no wake-ups.

## The content index and its memory

The first grep builds the content index: for every walked file, the
printable bigrams it holds and those one byte apart, as FFF builds them.
A grep reads only the files whose bigrams can hold its pattern (literal,
regex or fuzzy), plus those created since; a file edited since has its
bigrams kept beside the index. The build reads files in chunks of 4096 on
the blocking pool, each chunk its own builder merged by bit position.

`[ext.index] memory_mib` (default 128) bounds the content index of one
workspace: one is built only if its builders could not take more, the
whole of both and the chunk builders in flight beside them, counted with
the file list. Above that, grep reads every file. About 100k files fit in
the default ([perf/index.md](perf/index.md)). It does not bound the file
list itself: a walk of a very large root keeps every file it finds.

## Copied from FFF

`crates/ext-index/src/fff` is a pruned copy of FFF's search core
(`fff-search`, `fff-grep` and `fff-query-parser` 0.11.0 at 7353971, MIT,
its `LICENSE` beside it), owned here as `llm` owns its copy of rig: the
scorer over `neo_frizbee`, the bigram prefilter and its regex and fuzzy
queries, constraints, grep over byte slices and the query parser. Left
out: FFF's threads and global pools, its watcher, LMDB frecency and query
history, git status and recency, memory-mapped content, logging, the
definition classifier and the bindings. Paths live in one contiguous arena
instead of FFF's chunk arena of raw pointers; nothing in the crate is
`unsafe`. The builders' slabs are mapped straight from the OS
(`sys::ZeroedWords`): the allocator would keep a freed block that large
resident (on macOS, in its deferred-reclaim cache).
