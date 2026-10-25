# Workspace index

`index` is a default CLI feature and an extension (`crates/ext-index`),
disabled with `[ext] disable = ["index"]` or left out of a build without
the `index` feature. It keeps one file index per workspace in the daemon,
shared by every client and session working there, and answers from it:

| Name | Kind | What |
| --- | --- | --- |
| `files.search` | workspace query, at once | The best files for what is typed: an @-mention completing |
| `files.grep` | workspace query, streamed | The lines that match, one update per file, then a summary |
| `glob` | model tool | The files a glob matches, most recently modified first |
| `grep` | model tool | The lines that match, as ripgrep prints them |

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

The files a ripgrep walk of the workspace finds: inside git, what
`.gitignore`, `.ignore`, `.git/info/exclude` and the global excludes leave,
hidden files included, `.git` itself excluded; outside git, hidden files
and the usual tool and cache trees (`node_modules`, `target/debug`,
`.venv`, …) are left out too. Symbolic links are not followed. Paths that
are not UTF-8 are left out.

## Lifetime

The first query or call about a workspace opens its index: the walk runs
on the runtime's blocking pool (ripgrep's parallel walker, its threads
scoped to the walk), with the native watcher armed first so nothing that
changes during it is missed. Changes are applied as they arrive: a changed
file is looked at again, a changed directory walked again, a removed one
dropped; a change to an ignore file, a watcher overflow or rescan, or the
root going away, walks the whole root again. The watcher's callback only
queues events for a task on the runtime: rearming or dropping the watcher
from its own thread would wait on that thread.

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

`[ext.index] memory_mib` (default 128) bounds what one workspace's file
list and content index may take. A content index whose builders could take
more is not built, and one that turned out larger is dropped: grep then
reads every file. About 100k files fit in the default
([perf/index.md](perf/index.md)).

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
